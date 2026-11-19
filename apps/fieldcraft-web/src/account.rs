//! FieldCraft design operations through the WebOS account host.

use std::{cell::RefCell, rc::Rc};

use fieldcraft_engine::{AccountVersion, Session};
use fieldcraft_ui_egui::{
    ModelApp,
    files::{AccountFile, AccountFileFn},
};
use serde_json::{Value, json};
use wasm_bindgen::JsValue;

use crate::account_port::{AccountPort, field, string};

pub(crate) struct AccountView {
    port: AccountPort,
    action: Rc<RefCell<Option<AccountFile>>>,
    last_state: Value,
    pending: Option<Save>,
    dialog: Option<(String, Option<u64>)>,
    sequence: u64,
    lease: Option<String>,
    finalized: bool,
    revision: u64,
    observed: AccountVersion,
}

struct Save {
    ticket: String,
    document: String,
    revision: String,
    path: String,
    project: AccountVersion,
}

impl AccountView {
    pub(crate) fn new(port: AccountPort, session: &Session) -> Self {
        Self {
            port,
            action: Rc::default(),
            last_state: Value::Null,
            pending: None,
            dialog: None,
            sequence: 0,
            revision: 0,
            observed: session.account_version(),
            lease: None,
            finalized: false,
        }
    }

    pub(crate) fn design_service(&self) -> AccountFileFn {
        let action = self.action.clone();
        Box::new(move |request| {
            let mut next = action.try_borrow_mut().map_err(|_| "FieldCraft is processing a project request.")?;
            if next.is_some() {
                return Err("FieldCraft is processing a project request.".into());
            }
            *next = Some(request);
            Ok(())
        })
    }

    pub(crate) fn request_close(&self) {
        let _ = self.port.publish("close-intent", json!({}), None);
    }

    pub(crate) fn suspended(&self) -> bool {
        self.lease.is_some() || self.finalized
    }

    fn busy(&self) -> bool {
        self.pending.is_some() || self.dialog.is_some()
    }

    fn unfinished(app: &ModelApp) -> bool {
        app.busy().is_some() || app.dialog.is_some() || app.prompt.is_some()
    }

    fn state(&self, app: &ModelApp) -> Value {
        let session = &app.session;
        let unfinished = Self::unfinished(app);
        json!({"available":true,"documentId":session.opened().to_string(),"revision":self.revision.to_string(),
            "recoveryRevision":self.revision.to_string(),"dirty":session.is_dirty(),"capturable":!unfinished,
            "unfinishedEdit":unfinished,"autosaveEligible":false,"operationPending":self.busy()})
    }

    fn targets(&self, app: &ModelApp, packet: &JsValue) -> bool {
        let session = &app.session;
        number(packet, "documentId") == Some(session.opened()) && number(packet, "revision") == Some(self.revision)
    }

    fn track(&mut self, app: &ModelApp) {
        if !self.observed.matches(&app.session) {
            self.revision = self.revision.saturating_add(1);
            self.observed = app.session.account_version();
        }
    }

    pub(crate) fn poll(&mut self, app: &mut ModelApp) {
        self.track(app);
        for (kind, packet) in self.port.take_messages() {
            if let Err(error) = self.receive(app, &kind, &packet) {
                app.set_status(error, true);
            }
            self.track(app);
        }
        let action = self.action.borrow_mut().take();
        if let Some(action) = action
            && let Err(error) = self.project_action(app, action)
        {
            app.set_status(error, true);
        }
    }

    pub(crate) fn publish_state(&mut self, app: &ModelApp) -> Result<(), String> {
        self.track(app);
        let state = self.state(app);
        if state != self.last_state {
            self.port.publish("project-state", state.clone(), None)?;
            self.last_state = state;
        }
        Ok(())
    }

    fn receive(&mut self, app: &mut ModelApp, kind: &str, packet: &JsValue) -> Result<(), String> {
        if kind == "save-result" {
            return self.settle_save(app, packet);
        }
        let Some(id) = bounded(packet, "requestId", 36) else { return Ok(()) };
        match kind {
            "recovery-capture" => {
                let mut reply = json!({"requestId":id,"documentId":bounded(packet,"documentId",20),"revision":bounded(packet,"revision",20)});
                if self.targets(app, packet) && !Self::unfinished(app) && !self.suspended() {
                    match app.session.account_bytes() {
                        Ok(bytes) => return self.port.publish("recovery-captured", reply, Some(bytes)),
                        Err(error) => reply["message"] = json!(error.to_string()),
                    }
                } else {
                    // Let the host retire this draft request before its correlated reply arrives.
                    self.publish_state(app)?;
                    reply["message"] = json!("The FieldCraft project changed before recovery capture.");
                }
                self.port.publish("recovery-captured", reply, None)
            }
            "session-save" => self.capture_session_save(app, packet, id),
            "session" => self.session(app, packet, id),
            "dialog-result" => self.dialog_result(app, packet, id),
            "operation" => self.operation(app, packet, id),
            _ => Ok(()),
        }
    }

    fn next_id(&mut self) -> Result<String, String> {
        self.sequence = self.sequence.checked_add(1).ok_or("FieldCraft exhausted its request identifiers.")?;
        Ok(self.sequence.to_string())
    }

    fn request_dialog(&mut self, mut packet: Value, revision: Option<u64>) -> Result<(), String> {
        let id = self.next_id()?;
        packet["requestId"] = json!(id);
        self.port.publish("dialog", packet, None)?;
        self.dialog = Some((id, revision));
        Ok(())
    }

    fn project_action(&mut self, app: &mut ModelApp, action: AccountFile) -> Result<(), String> {
        if self.suspended() || self.busy() {
            return Err("Finish the pending FieldCraft project operation first.".into());
        }
        let session = &app.session;
        match action {
            AccountFile::Sample(name) => self.request_dialog(json!({"operation":"new","settings":json!({"sample":name}).to_string()}), None),
            AccountFile::New => self.request_dialog(json!({"operation":"new","settings":"{}"}), None),
            AccountFile::Open(path) => self.request_dialog(json!({"operation":"open","path":path}), None),
            AccountFile::Save | AccountFile::SaveAs(_) => {
                if Self::unfinished(app) {
                    return Err("Finish the current FieldCraft design command before saving.".into());
                }
                let suggested = match action {
                    AccountFile::Save => {
                        if let Some(path) = &session.path {
                            return self.send_save(app, path.clone(), "save", json!({}));
                        }
                        format!("{}.fieldcraft", app.model_name())
                    }
                    AccountFile::SaveAs(name) => name,
                    _ => return Err("Invalid FieldCraft save operation.".into()),
                };
                self.request_dialog(json!({"operation":"save","suggested":suggested}), Some(self.revision))
            }
        }
    }

    fn dialog_result(&mut self, app: &ModelApp, packet: &JsValue, id: String) -> Result<(), String> {
        if !self.dialog.as_ref().is_some_and(|(expected, _)| expected == &id) {
            return Ok(());
        }
        let Some((_, revision)) = self.dialog.take() else { return Ok(()) };
        match bounded(packet, "outcome", 16).as_deref() {
            Some("failed") => Err(bounded(packet, "message", 250).unwrap_or_else(|| "The FieldCraft dialog failed.".into())),
            Some("destination") if revision.is_some() => {
                if revision != Some(self.revision) || Self::unfinished(app) || self.suspended() {
                    return Err("The project changed while choosing its save destination.".into());
                }
                let path = bounded(packet, "path", 2048).ok_or("Invalid FieldCraft save destination.")?;
                self.send_save(app, path, "save", json!({}))
            }
            _ => Ok(()),
        }
    }

    fn capture_session_save(&mut self, app: &ModelApp, packet: &JsValue, id: String) -> Result<(), String> {
        let path = bounded(packet, "path", 2048).ok_or("Invalid FieldCraft save destination.")?;
        let mut reply = json!({"requestId":id,"path":path,"documentId":bounded(packet,"documentId",20),
            "reviewedRevision":bounded(packet,"revision",20)});
        if self.busy() || self.suspended() || Self::unfinished(app) || !self.targets(app, packet) {
            reply["outcome"] = json!("busy");
            return self.port.publish("session-save-result", reply, None);
        }
        if let Err(error) = self.send_save(app, path, "session-save-result", reply.clone()) {
            reply["outcome"] = json!("failed");
            reply["message"] = json!(error);
            self.port.publish("session-save-result", reply, None)?;
        }
        Ok(())
    }

    fn send_save(&mut self, app: &ModelApp, path: String, kind: &str, mut reply: Value) -> Result<(), String> {
        let bytes = app.session.account_bytes().map_err(|error| error.to_string())?;
        let project = app.session.account_version();
        let ticket = self.next_id()?;
        let document = app.session.opened().to_string();
        let revision = self.revision.to_string();
        reply["outcome"] = json!("captured");
        reply["ticket"] = json!(ticket);
        reply["documentId"] = json!(document);
        reply["revision"] = json!(revision);
        reply["path"] = json!(path);
        self.port.publish(kind, reply, Some(bytes))?;
        self.pending = Some(Save { ticket, document, revision, path, project });
        Ok(())
    }

    fn settle_save(&mut self, app: &mut ModelApp, packet: &JsValue) -> Result<(), String> {
        let matching = self.pending.as_ref().is_some_and(|save| {
            bounded(packet, "ticket", 20).as_deref() == Some(save.ticket.as_str())
                && bounded(packet, "documentId", 20).as_deref() == Some(save.document.as_str())
                && bounded(packet, "revision", 20).as_deref() == Some(save.revision.as_str())
                && bounded(packet, "path", 2048).as_deref() == Some(save.path.as_str())
        });
        let outcome = bounded(packet, "outcome", 16);
        if !matching || !matches!(outcome.as_deref(), Some("written" | "failed" | "unknown")) {
            return Ok(());
        }
        let Some(save) = self.pending.take() else { return Ok(()) };
        let adopted = outcome.as_deref() == Some("written") && app.session.confirm_account_save(save.path.clone(), save.project).is_ok();
        self.port.publish(
            "save-settled",
            json!({"ticket":save.ticket,"documentId":save.document,"revision":save.revision,
            "path":save.path,"adopted":adopted,"projectState":self.state(app)}),
            None,
        )
    }

    fn session(&mut self, app: &mut ModelApp, packet: &JsValue, id: String) -> Result<(), String> {
        let operation = bounded(packet, "operation", 16).ok_or("Invalid FieldCraft session operation.")?;
        let lease = bounded(packet, "lease", 64).filter(|value| !value.is_empty()).ok_or("Invalid FieldCraft session lease.")?;
        let accepted = match operation.as_str() {
            "pause"
                if !self.suspended()
                    && !self.busy()
                    && !Self::unfinished(app)
                    && self.targets(app, packet)
                    && field(packet, "dirty").as_bool() == Some(app.session.is_dirty())
                    && field(packet, "unfinishedEdit").as_bool() == Some(false) =>
            {
                self.lease = Some(lease.clone());
                true
            }
            "release" | "finalize" if self.lease.as_deref() == Some(lease.as_str()) => {
                self.lease = None;
                self.finalized = operation == "finalize";
                true
            }
            _ => false,
        };
        self.port.publish(
            "session-result",
            json!({"requestId":id,"operation":operation,"lease":lease,"ok":accepted,
            "message":if accepted {""} else {"The FieldCraft project changed after close review."}}),
            None,
        )
    }

    fn operation(&self, app: &mut ModelApp, packet: &JsValue, id: String) -> Result<(), String> {
        let document = bounded(packet, "documentId", 20).ok_or("Invalid FieldCraft project target.")?;
        let mut reply = json!({"requestId":id,"documentId":document});
        let path = field(packet, "path");
        let path = if path.is_null() { None } else { Some(path.as_string().ok_or("Invalid FieldCraft project binding.")?) };
        let session = &app.session;
        let result = if self.suspended() || document != session.opened().to_string() || path != session.path {
            Err("The FieldCraft operation does not match the current design.".into())
        } else {
            let command = bounded(packet, "command", 64).ok_or("Invalid FieldCraft project operation.")?;
            let args = bounded(packet, "args", 4096).ok_or("Invalid FieldCraft operation arguments.")?;
            let args: Value = serde_json::from_str(&args).map_err(|_| "Invalid FieldCraft operation arguments.")?;
            let fields = args.as_object().ok_or("FieldCraft operation arguments must be an object.")?;
            if command == "document.inspect" && fields.is_empty() {
                Ok(json!({"title":app.model_name(),"nodes":session.model.nodes().len(), "elements":session.model.elements().len(),
                    "loads":session.model.loads().keys().collect::<Vec<_>>(), "constraints":session.model.boundary_conditions().keys().collect::<Vec<_>>(),
                    "steps":session.model.steps().iter().map(|s| &s.name).collect::<Vec<_>>(),"dirty":session.is_dirty()}))
            } else if self.busy() || Self::unfinished(app) {
                Err("Finish the pending FieldCraft design command first.".into())
            } else {
                match command.as_str() {
                    "edit.undo" | "solve.static" if fields.is_empty() => app.run(&command, args),
                    "mesh.node.add"
                        if fields.len() == 2
                            && args.get("label").and_then(Value::as_u64).is_some_and(|n| n > 0 && n <= u64::from(u32::MAX))
                            && args.get("xyz").and_then(Value::as_array).is_some_and(|p| {
                                p.len() == 3 && p.iter().all(|axis| axis.as_f64().is_some_and(|n| n.is_finite() && n.abs() <= 1e9))
                            }) =>
                    {
                        app.run(&command, args)
                    }
                    _ => Err("Unsupported FieldCraft design operation.".into()),
                }
            }
        };
        match result {
            Ok(value) => reply["result"] = json!(value.to_string()),
            Err(error) => reply["message"] = json!(error),
        }
        self.port.publish("operation-result", reply, None)
    }
}

fn bounded(packet: &JsValue, field: &str, maximum: usize) -> Option<String> {
    string(packet, field).filter(|text| text.len() <= maximum)
}

fn number(packet: &JsValue, field: &str) -> Option<u64> {
    bounded(packet, field, 20)?.parse().ok()
}

pub(crate) async fn open(port: &AccountPort) -> Result<(Session, Value), String> {
    let request = port.start().await?;
    let session = if let Some(bytes) = request.bytes {
        match Session::open_account_model(request.path.clone(), &bytes, request.recover) {
            Ok(session) => session,
            Err(error) => {
                let error = error.to_string();
                port.open_failed(&request.request_id, &error);
                return Err(error);
            }
        }
    } else {
        let sample = request.settings.get("sample").and_then(Value::as_str);
        let allowed = request.settings.as_object().is_some_and(|s| s.is_empty() || s.len() == 1 && sample.is_some());
        let session =
            if allowed { Session::new_account_model(sample).map_err(|e| e.to_string()) } else { Err("Unknown FieldCraft model settings.".into()) };
        match session {
            Ok(s) => s,
            Err(message) => {
                port.open_failed(&request.request_id, &message);
                return Err(message);
            }
        }
    };
    let identity = json!({
        "requestId":request.request_id, "documentId":session.opened().to_string(),
        "revision":"0", "path":session.path, "ok":true,
    });
    Ok((session, identity))
}
