//! Optional host persistence at the interactive file boundary.
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use photocraft_doc::DocId;
use serde_json::{Value, json};

use crate::PhotocraftApp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Save,
    SaveAs,
    Export,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::SaveAs => "saveAs",
            Self::Export => "export",
        }
    }
}

/// The host replies only after persistence. A rejected promise (including cancellation) is Err.
pub type PersistFn = Box<dyn FnMut(String, Vec<u8>, Action, Sender<Result<(), String>>)>;

pub(crate) struct Pending {
    pub save: Option<(DocId, u64)>,
    path: String,
    warnings: Vec<String>,
    receiver: Receiver<Result<(), String>>,
}

pub(crate) struct Opening {
    before: Vec<DocId>,
    name: String,
    reply: Sender<Result<(), String>>,
}

impl PhotocraftApp {
    /// A CMS owns one asset at a time. Refuse unsaved replacement, then retain the old
    /// document until decoding and any profile/Camera Raw prompts have succeeded.
    pub fn open_host_document(&mut self, name: &str, bytes: &[u8]) -> Result<Receiver<Result<(), String>>, String> {
        if crate::preset_files_ui::is_preset_file(name) {
            return Err("Choose a document or image, not a preset file".into());
        }
        if self.has_unsaved_work() || self.saving() {
            return Err("Save or close unsaved documents before loading another CMS asset".into());
        }
        if self.host_open.is_some()
            || self.file_dialog_open()
            || !self.ui.dialogs.is_empty()
            || self.discard.is_some()
            || crate::open_prompt::waiting(self)
            || self.camera_raw.is_some()
        {
            return Err("Finish the current dialog before loading another CMS asset".into());
        }
        let before = self.session.documents().iter().map(|st| st.doc.id).collect();
        self.open_bytes(name, bytes)?;
        let (reply, receiver) = std::sync::mpsc::channel();
        self.host_open = Some(Opening { before, name: crate::file_open::display_name(name), reply });
        self.poll_host_open();
        Ok(receiver)
    }

    pub(crate) fn poll_host_open(&mut self) {
        if crate::open_prompt::waiting(self) || self.camera_raw.is_some() || !self.ui.dialogs.is_empty() {
            return;
        }
        let Some(open) = self.host_open.take() else { return };
        let arrived = self.session.documents().iter().find(|st| !open.before.contains(&st.doc.id)).map(|st| st.doc.id);
        let result: Result<(), String> = if let Some(doc) = arrived {
            let _ = self.with_document(doc, |app| {
                if let Some(st) = app.session.active_mut() {
                    std::sync::Arc::make_mut(&mut st.doc).name = open.name;
                }
                Ok(())
            });
            // Never remove a document edited while a host request was pending.
            for i in (0..self.session.documents().len()).rev() {
                if self.session.documents().get(i).is_some_and(|st| open.before.contains(&st.doc.id) && !st.is_dirty()) {
                    self.session.close(i);
                }
            }
            self.sync_views();
            Ok(())
        } else {
            Err("Document loading was cancelled or the file is not an editable document".into())
        };
        if let Err(e) = &result {
            self.ui.status = e.clone();
            self.ui.status_error = true;
        }
        let _ = open.reply.send(result);
    }

    /// CMS saves use the native editable format so saving cannot silently flatten layers.
    pub(crate) fn save_to_host(&mut self, action: Action) -> Result<Value, String> {
        if action == Action::Save && self.session.is_enabled("layer.smartObjects.saveContents") {
            return self.run("layer.smartObjects.saveContents", json!({}));
        }
        if self.saving() || self.host_open.is_some() {
            return Err("Wait for the current save to finish".into());
        }
        let st = self.session.active().ok_or("no document")?;
        let name = std::path::Path::new(&st.doc.name).with_extension("pcraft").to_string_lossy().into_owned();
        let saved = (st.doc.id, st.revision);
        let export = self.services.export.as_ref().ok_or("no exporter configured")?;
        let (bytes, warnings) = export(&st.doc, &name, &self.save_settings())?;
        self.persist_to_host(name, bytes, warnings, action, Some(saved))?;
        Ok(json!({"pending": true}))
    }

    fn persist_to_host(&mut self, path: String, bytes: Vec<u8>, warnings: Vec<String>, action: Action, save: Option<(DocId, u64)>) -> Result<(), String> {
        let persist = self.services.host_persist.as_mut().ok_or("no host persistence configured")?;
        let (tx, receiver) = std::sync::mpsc::channel();
        persist(path.clone(), bytes, action, tx);
        self.host_writes.push(Pending { save, path, warnings, receiver });
        self.ui.status = "Waiting for CMS persistence…".into();
        self.ui.status_error = false;
        Ok(())
    }

    /// Returns true when persistence is pending; callers must not announce success yet.
    pub(crate) fn export_to_host(&mut self, path: &str, bytes: &[u8], warnings: &[String]) -> Result<bool, String> {
        if self.services.host_persist.is_none() {
            return Ok(false);
        }
        self.persist_to_host(path.into(), bytes.to_vec(), warnings.to_vec(), Action::Export, None)?;
        Ok(true)
    }

    pub fn poll_host_writes(&mut self, ctx: &egui::Context) {
        let mut waiting = Vec::new();
        for pending in std::mem::take(&mut self.host_writes) {
            let result = match pending.receiver.try_recv() {
                Err(TryRecvError::Empty) => {
                    waiting.push(pending);
                    continue;
                }
                Err(TryRecvError::Disconnected) => Err("CMS persistence ended without confirmation".into()),
                Ok(result) => result,
            };
            match result {
                Ok(()) => {
                    if let Some((doc, revision)) = pending.save {
                        self.saved(doc, revision, &pending.path, &pending.warnings, false);
                        crate::discard_ui::saved_document(self, ctx, doc);
                    } else {
                        self.ui.status = format!("Exported {} to CMS", pending.path);
                        self.ui.status_error = false;
                        crate::notices::io_warnings(self, "CMS export", &pending.warnings);
                    }
                }
                Err(error) => {
                    self.ui.status = format!("CMS could not persist {}: {error}. Retry Save or Export.", pending.path);
                    self.ui.status_error = true;
                }
            }
        }
        self.host_writes.extend(waiting);
        if !self.host_writes.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Services, menus};
    use std::{cell::RefCell, rc::Rc};

    fn app() -> PhotocraftApp {
        PhotocraftApp::new(
            photocraft_engine::Session::new(),
            Services {
                import: Some(Box::new(|name, bytes, options| {
                    photocraft_io::import_with_options(name, bytes, options).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
                })),
                export: Some(Box::new(|doc, path, settings| {
                    photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
                })),
                ..Default::default()
            },
        )
    }

    #[test]
    fn host_saves_wait_for_confirmation_keep_layers_and_do_not_clear_later_edits() {
        for action in [Action::Save, Action::SaveAs] {
            for depth in [8, 16, 32] {
                let mut app = app();
                app.run("file.new", json!({"width": 2, "height": 2, "depth": depth})).unwrap();
                app.run("layer.new.layer", json!({"name": "CMS layer"})).unwrap();
                let captured = Rc::new(RefCell::new(None));
                let capture = captured.clone();
                app.services.host_persist = Some(Box::new(move |name, bytes, action, reply| {
                    *capture.borrow_mut() = Some((name, bytes, action, reply));
                }));
                let revision = app.session.active().unwrap().revision;
                let ctx = egui::Context::default();
                let command = if action == Action::Save { "file.save" } else { "file.saveAs" };
                let result = menus::invoke(&mut app, &ctx, command, json!({})).unwrap();
                assert_eq!(result["pending"], true);
                assert!(app.has_unsaved_work());
                assert!(app.saving());
                assert!(menus::invoke(&mut app, &ctx, "file.save", json!({})).is_err());
                let (name, bytes, routed, reply) = captured.borrow_mut().take().unwrap();
                assert_eq!(routed, action);
                assert!(name.ends_with(".pcraft"));
                let reopened = photocraft_io::import(&name, &bytes).unwrap().document;
                assert_eq!(reopened.layers, app.session.active().unwrap().doc.layers);
                assert_eq!(reopened.depth.bits(), depth);
                app.run("layer.new.layer", json!({"name": "Later edit"})).unwrap();
                reply.send(Ok(())).unwrap();
                app.poll_host_writes(&ctx);
                assert!(!app.saving());
                assert_eq!(app.session.active().unwrap().saved_revision, revision);
                assert!(app.has_unsaved_work());
                assert!(app.ui.status.starts_with("Saved"));
            }
        }
    }

    #[test]
    fn rejected_cancelled_or_dropped_uploads_preserve_unsaved_identity() {
        for result in [Some(Err("CMS permission denied".into())), Some(Err("cancelled".into())), None] {
            let mut app = app();
            app.run("file.new", json!({"width": 2, "height": 2})).unwrap();
            app.run("layer.new.layer", json!({})).unwrap();
            let before = app.session.active().unwrap().saved_revision;
            app.services.host_persist = Some(Box::new(move |_, _, _, reply| {
                if let Some(result) = &result {
                    reply.send(result.clone()).unwrap();
                }
            }));
            app.save_to_host(Action::Save).unwrap();
            app.poll_host_writes(&egui::Context::default());
            assert!(app.has_unsaved_work());
            assert_eq!(app.session.active().unwrap().saved_revision, before);
            assert!(app.session.active().unwrap().path.is_none());
            assert!(app.ui.status_error);
        }
    }

    #[test]
    fn acknowledged_save_clears_only_the_captured_document() {
        let mut app = app();
        app.run("file.new", json!({"width": 2, "height": 2})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        let id = app.session.active().unwrap().doc.id;
        app.services.host_persist = Some(Box::new(|_, _, _, reply| {
            reply.send(Ok(())).unwrap();
        }));
        app.save_to_host(Action::Save).unwrap();
        assert!(app.has_unsaved_work(), "the UI has not consumed the acknowledgement yet");
        app.poll_host_writes(&egui::Context::default());
        assert!(!app.has_unsaved_work());
        assert_eq!(app.session.active().unwrap().doc.id, id);
        assert!(app.session.active().unwrap().path.as_deref().unwrap().ends_with(".pcraft"));
    }

    #[test]
    fn export_is_distinct_and_never_marks_the_document_saved() {
        let mut app = app();
        app.run("file.new", json!({"width": 2, "height": 2})).unwrap();
        app.run("layer.new.layer", json!({})).unwrap();
        app.services.host_persist = Some(Box::new(|name, bytes, action, reply| {
            assert_eq!(action, Action::Export);
            assert_eq!(name, "preview.png");
            assert_eq!(bytes, b"encoded");
            reply.send(Ok(())).unwrap();
        }));
        assert!(app.export_to_host("preview.png", b"encoded", &[]).unwrap());
        app.poll_host_writes(&egui::Context::default());
        assert!(app.has_unsaved_work());
        assert!(app.session.active().unwrap().path.is_none());
        assert!(app.ui.status.starts_with("Exported"));
    }

    fn file() -> Vec<u8> {
        let mut app = app();
        app.run("file.new", json!({"width": 2, "height": 2})).unwrap();
        photocraft_io::export(&app.session.active().unwrap().doc, "image.pcraft", &Default::default()).unwrap().bytes
    }

    #[test]
    fn initial_loading_opens_once_errors_leave_editor_usable_and_dirty_replacement_is_rejected() {
        let mut app = app();
        let bytes = file();
        let loaded = app.open_host_document("initial.pcraft", &bytes).unwrap();
        assert_eq!(loaded.recv().unwrap(), Ok(()));
        app.poll_host_open();
        assert_eq!(app.session.documents().len(), 1);
        assert!(app.open_host_document("broken.png", b"bad image").is_err());
        assert_eq!(app.session.documents().len(), 1);
        let loaded = app.open_host_document("replacement.pcraft", &bytes).unwrap();
        assert_eq!(loaded.recv().unwrap(), Ok(()));
        assert_eq!(app.session.documents().len(), 1);
        assert_eq!(app.session.active().unwrap().doc.name, "replacement.pcraft");
        app.run("layer.new.layer", json!({})).unwrap();
        assert!(app.open_host_document("would lose edits.pcraft", &bytes).is_err());
        assert!(app.has_unsaved_work());
        assert_eq!(app.session.active().unwrap().doc.name, "replacement.pcraft");
    }

    #[test]
    fn cancelling_the_cms_picker_leaves_the_document_unchanged() {
        let mut app = app();
        app.open_host_document("initial.pcraft", &file()).unwrap().recv().unwrap().unwrap();
        let id = app.session.active().unwrap().doc.id;
        let (dialog, _) = crate::file_dialog::fake(vec![None]);
        app.services.file_dialog = Some(dialog);
        app.services.host_persist = Some(Box::new(|_, _, _, _| panic!("no persistence on cancelled Open")));
        menus::invoke(&mut app, &egui::Context::default(), "file.open", json!({})).unwrap();
        app.poll_file_dialog(&egui::Context::default(), None);
        assert_eq!(app.session.documents().len(), 1);
        assert_eq!(app.session.active().unwrap().doc.id, id);
        assert!(!app.ui.status_error);
    }
}
