use crate::connect;
use crate::editor::EditorHandle;
use crate::ot::OTServer;
use crate::types::{EditorProtocolMessage, EditorTextDelta, RevisionedEditorTextDelta, TextDelta};
use anyhow::Result;
use automerge::{
    patches::TextRepresentation,
    sync::{Message as AutomergeSyncMessage, State as SyncState, SyncDoc},
    transaction::Transactable,
    AutoCommit, ObjType, Patch, PatchLog, ReadDoc,
};
use rand::Rng;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

// These messages are sent to the task that owns the document.
pub enum DocMessage {
    GetContent {
        response_tx: oneshot::Sender<Result<String>>,
    },
    Open,
    Close,
    RandomEdit,
    RevDelta(RevisionedEditorTextDelta),
    Delta(TextDelta),
    ReceiveSyncMessage {
        message: AutomergeSyncMessage,
        state: SyncState,
        response_tx: oneshot::Sender<SyncState>,
    },
    GenerateSyncMessage {
        state: SyncState,
        response_tx: oneshot::Sender<(SyncState, Option<AutomergeSyncMessage>)>,
    },
    NewEditorConnection(EditorHandle),
}

impl fmt::Debug for DocMessage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let repr = match self {
            DocMessage::GetContent { .. } => "get content",
            DocMessage::Open => "open",
            DocMessage::Close => "close",
            DocMessage::RandomEdit => "random edit",
            DocMessage::RevDelta(_) => "delta from editor",
            DocMessage::Delta(_) => "delta from peer",
            DocMessage::ReceiveSyncMessage { .. } => "<automerge internal sync rcv>",
            DocMessage::GenerateSyncMessage { .. } => "<automerge internal sync gen>",
            DocMessage::NewEditorConnection(_) => "editor connected",
        };
        write!(f, "{repr}")
    }
}

impl From<EditorProtocolMessage> for DocMessage {
    fn from(rpc_message: EditorProtocolMessage) -> Self {
        match rpc_message {
            EditorProtocolMessage::Open { uri: _ } => DocMessage::Open,
            EditorProtocolMessage::Close { uri: _ } => DocMessage::Close,
            EditorProtocolMessage::Edit { uri: _, delta } => DocMessage::RevDelta(delta),
        }
    }
}

type DocMessageSender = mpsc::Sender<DocMessage>;
type DocChangedSender = broadcast::Sender<()>;
type DocChangedReceiver = broadcast::Receiver<()>;

/// Encapsulates the Automerge `AutoCommit` and provides a generic interface,
/// s.t. we don't need to worry about automerge internals elsewhere.
#[derive(Debug, Default)]
pub struct Document {
    doc: AutoCommit,
}

impl Document {
    fn receive_sync_message_log_patches(
        &mut self,
        message: AutomergeSyncMessage,
        peer_state: &mut SyncState,
    ) -> Vec<Patch> {
        let mut patch_log = PatchLog::active(TextRepresentation::String);
        self.doc
            .sync()
            .receive_sync_message_log_patches(peer_state, message, &mut patch_log)
            .expect("Failed to apply sync message to Automerge document");
        self.doc.make_patches(&mut patch_log)
    }

    fn receive_sync_message(&mut self, message: AutomergeSyncMessage, peer_state: &mut SyncState) {
        self.doc
            .sync()
            .receive_sync_message(peer_state, message)
            .expect("Failed to apply sync message to Automerge document");
    }

    fn generate_sync_message(
        &mut self,
        peer_state: &mut SyncState,
    ) -> Option<AutomergeSyncMessage> {
        self.doc.sync().generate_sync_message(peer_state)
    }

    fn text_obj(&self) -> Result<automerge::ObjId> {
        let text_obj = self
            .doc
            .get(automerge::ROOT, "text")
            .expect("Failed to get text key from Automerge document");
        if let Some((automerge::Value::Object(ObjType::Text), text_obj)) = text_obj {
            Ok(text_obj)
        } else {
            Err(anyhow::anyhow!(
                "Automerge document doesn't have a text object, so I can't provide it"
            ))
        }
    }

    fn apply_delta_to_doc(&mut self, delta: &EditorTextDelta) {
        let text_obj = self
            .text_obj()
            .expect("Couldn't get automerge text object, so not able to modify it");
        let mut offset = 0i32;
        let text = self
            .current_content()
            .expect("Should have initialized text before performing random edit");
        for op in &delta.0 {
            let (start, length) = op.range.as_relative(&text);
            self.doc
                .splice_text(
                    text_obj.clone(),
                    (start as i32 + offset) as usize,
                    length as isize,
                    &op.replacement,
                )
                .expect("Failed to splice Automerge text object");
            offset -= length as i32;
            offset += op.replacement.chars().count() as i32;
        }
    }

    fn current_content(&self) -> Result<String> {
        self.text_obj().map(|to| {
            self.doc
                .text(to)
                .expect("Failed to get string from Automerge text object")
        })
    }

    fn initialize_text(&mut self, text: &str) {
        let text_obj = self
            .doc
            .put_object(automerge::ROOT, "text", ObjType::Text)
            .expect("Failed to initialize text object in Automerge document");
        self.doc
            .splice_text(text_obj, 0, 0, text)
            .expect("Failed to splice text into Automerge text object");
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct EditorId(usize);

/// This Actor is responsible for applying changes to the document asynchronously.
///
/// Any DocMessage that is emitted via DocumentActorHandle should have an effect eventually.
pub struct DocumentActor {
    doc_message_rx: mpsc::Receiver<DocMessage>,
    doc_changed_ping_tx: DocChangedSender,
    editor_clients: HashMap<EditorId, EditorHandle>,
    /// If we have an ot_server, it means that an editor is connected.
    ot_server: Option<OTServer>,
    /// The Document is the main I/O managed resource of this actor.
    crdt_doc: Document,
    file_path: PathBuf,
}

impl DocumentActor {
    #[must_use]
    fn new(
        doc_message_rx: mpsc::Receiver<DocMessage>,
        doc_changed_ping_tx: DocChangedSender,
        file_path: PathBuf,
    ) -> Self {
        Self {
            doc_message_rx,
            doc_changed_ping_tx,
            editor_clients: HashMap::default(),
            file_path,
            ot_server: None,
            crdt_doc: Document::default(),
        }
    }
    async fn handle_message(&mut self, message: DocMessage) {
        // TODO: Show the type in the debug message, or implement Debug for DocMessage.
        debug!("Handling doc message: {message:?}");
        match message {
            DocMessage::GetContent { response_tx } => {
                response_tx
                    .send(self.current_content())
                    .expect("Failed to send content to response channel");
            }
            DocMessage::Open => {
                self.ot_server = Some(OTServer::new(
                    self.current_content()
                        .expect("Should have initialized text before initializing the document"),
                ));
            }
            DocMessage::Close => {
                self.ot_server = None;
            }
            DocMessage::RandomEdit => {
                let delta = self.random_delta();
                let text = self
                    .current_content()
                    .expect("Should have initialized text before performing random edit");
                let ed_delta = EditorTextDelta::from_delta(delta.clone(), &text);
                self.apply_delta_to_doc(&ed_delta);
                self.process_crdt_delta_in_ot(delta).await;
            }
            DocMessage::Delta(delta) => {
                let text = self
                    .current_content()
                    .expect("Should have initialized text before performing random edit");
                let editor_delta = EditorTextDelta::from_delta(delta.clone(), &text);
                self.apply_delta_to_doc(&editor_delta);
                self.process_crdt_delta_in_ot(delta).await;
            }
            DocMessage::RevDelta(rev_delta) => {
                debug!("Handling RevDelta from editor: {:#?}", rev_delta);
                let (editor_delta_for_crdt, rev_deltas_for_editor) =
                    self.apply_delta_to_ot(rev_delta);

                self.apply_delta_to_doc(&editor_delta_for_crdt);
                self.send_deltas_to_editor(rev_deltas_for_editor).await;
            }
            DocMessage::ReceiveSyncMessage {
                message,
                state: mut peer_state,
                response_tx,
            } => {
                if let Some(patches) = self.apply_sync_message_to_doc(message, &mut peer_state) {
                    self.process_crdt_patches_in_ot(patches).await;
                }
                response_tx
                    .send(peer_state)
                    .expect("Failed to send peer state in response to ReceiveSyncMessage");
            }
            DocMessage::GenerateSyncMessage {
                state: mut peer_state,
                response_tx,
            } => {
                let message = self.crdt_doc.generate_sync_message(&mut peer_state);
                response_tx.send((peer_state, message)).expect(
                    "Failed to send peer state and sync message in response to GenerateSyncMessage",
                );
            }
            DocMessage::NewEditorConnection(editor_handle) => {
                // TODO: if we use more than one ID, we should now easily have multiple editors.
                // Modulo managing the OT server for each of them per file...
                self.editor_clients.insert(EditorId(0), editor_handle);
            }
        }
    }

    fn apply_sync_message_to_doc(
        &mut self,
        message: AutomergeSyncMessage,
        peer_state: &mut SyncState,
    ) -> Option<Vec<Patch>> {
        let result = if self.ot_server.is_some() {
            let patches = self
                .crdt_doc
                .receive_sync_message_log_patches(message, peer_state);
            Some(patches)
        } else {
            self.crdt_doc.receive_sync_message(message, peer_state);
            None
        };
        let _ = self.doc_changed_ping_tx.send(());
        result
    }

    async fn send_deltas_to_editor(&mut self, rev_deltas: Vec<RevisionedEditorTextDelta>) {
        for rev_delta in rev_deltas {
            debug!("Sending RevDelta to socket: {:#?}", rev_delta);

            self.send_to_editors(rev_delta).await;
        }
    }

    fn apply_delta_to_ot(
        &mut self,
        rev_editor_delta: RevisionedEditorTextDelta,
    ) -> (EditorTextDelta, Vec<RevisionedEditorTextDelta>) {
        let text = self
            .current_content()
            .expect("Should have initialized text before performing random edit");
        let ot_server = self
            .ot_server
            .as_mut()
            .expect("No editor connected, where does this delta come from?");
        let (delta_for_crdt, rev_deltas_for_editor) =
            ot_server.apply_editor_operation(rev_editor_delta);

        let editor_delta_for_crdt = EditorTextDelta::from_delta(delta_for_crdt, &text);
        (editor_delta_for_crdt, rev_deltas_for_editor)
    }

    fn random_delta(&self) -> TextDelta {
        let text = self
            .current_content()
            .expect("Should have initialized text before performing random edit");
        let options = ["d", "ü", "🥕", "💚", "\n"];
        let random_text: String = (1..5)
            .map(|_| {
                let random_option = rand::thread_rng().gen_range(0..options.len());
                options[random_option]
            })
            .collect();
        let text_length = text.chars().count();
        let random_position = rand::thread_rng().gen_range(0..=text_length);

        let mut delta = TextDelta::default();
        delta.retain(random_position);
        delta.insert(&random_text);

        // TODO: Delete the end/beginning of the content on purpose sometimes!
        let mut deletion_length = 0;
        if (text_length - random_position) > 0 {
            deletion_length = rand::thread_rng().gen_range(0..(text_length - random_position));
            deletion_length = deletion_length.min(3);
        }
        delta.delete(deletion_length);

        delta
    }

    async fn process_crdt_patches_in_ot(&mut self, patches: Vec<Patch>) {
        debug!(?patches);
        for patch in patches {
            match patch.action.try_into() {
                Ok(delta) => {
                    self.process_crdt_delta_in_ot(delta).await;
                }
                Err(e) => {
                    warn!("Failed to convert patch to delta: {:#?}", e);
                }
            }
        }
    }

    async fn process_crdt_delta_in_ot(&mut self, delta: TextDelta) {
        if let Some(ot_server) = &mut self.ot_server {
            let rev_text_delta_for_editor = ot_server.apply_crdt_change(delta);
            self.send_to_editors(rev_text_delta_for_editor).await;
        }
    }

    async fn send_to_editors(&mut self, rev_delta: RevisionedEditorTextDelta) {
        let message = EditorProtocolMessage::Edit {
            uri: format!("file://{}", self.file_path.display()),
            delta: rev_delta,
        };

        for (_id, handle) in self.editor_clients.iter_mut() {
            handle.send(message.clone()).await;
        }
    }

    fn write_current_content_to_file(&mut self) {
        let content = self.current_content();
        if let Ok(text) = content {
            debug!(current_text__ = text);
            if let Some(ot_server) = &mut self.ot_server {
                debug!(current_ot_doc = ot_server.current_content());
            } else {
                std::fs::write(&self.file_path, &text).expect("Could not write to file");
            }
        }
    }

    /// Reading in the file is a preparatory step, before kicking off the actor.
    fn read_current_content_from_file(&mut self) {
        // Create the file if it doesn't exist.
        if !self.file_path.exists() {
            std::fs::write(&self.file_path, "").expect("Could not create file");
        }

        if let Ok(text) = std::fs::read_to_string(&self.file_path) {
            self.crdt_doc.initialize_text(&text);
        } else {
            // TODO: Look at *why* we couldn't read the file.
            panic!("Could not read file {}", self.file_path.display());
        }
    }

    fn current_content(&self) -> Result<String> {
        self.crdt_doc.current_content()
    }

    fn apply_delta_to_doc(&mut self, delta: &EditorTextDelta) {
        self.crdt_doc.apply_delta_to_doc(delta);
        let _ = self.doc_changed_ping_tx.send(());
    }

    async fn run(&mut self) {
        while let Some(message) = self.doc_message_rx.recv().await {
            self.handle_message(message).await;
            self.write_current_content_to_file();
        }
        panic!("Channel towards document task has been closed");
    }
}

/// This handle knows how to talk to the DocumentActor and provides an interface for doing so.
///
/// The main iterfaces for doing so is through through sending `DocMessage`s with `send_message`.
/// An alternative pathway is to subscribe to documents changes through `subscribe_document_changes`.
///
/// The rest of the methods are used for instrumentation (e.g. by the fuzzer).
#[derive(Clone)]
pub struct DocumentActorHandle {
    doc_message_tx: DocMessageSender,
    doc_changed_ping_tx: DocChangedSender,
}

impl DocumentActorHandle {
    pub fn new(file_path: &Path, host: bool) -> Self {
        // The document task will receive messages on this channel.
        let (doc_message_tx, doc_message_rx) = mpsc::channel(1);

        // The document task will send a ping on this channel whenever it changes.
        // The sync tasks will subscribe to it, and react to it by syncing with the peers.
        let (doc_changed_ping_tx, _doc_changed_ping_rx) = broadcast::channel::<()>(1);

        let mut actor = DocumentActor::new(
            doc_message_rx,
            doc_changed_ping_tx.clone(),
            file_path.into(),
        );

        // Initialize the text from the file_path, if this is the document owned by the host.
        if host {
            actor.read_current_content_from_file();
        }

        tokio::spawn(async move { actor.run().await });

        Self {
            doc_message_tx,
            doc_changed_ping_tx,
        }
    }

    /// The TCP and socket connections will send messages through this when they receive something.
    pub async fn send_message(&self, message: DocMessage) {
        self.doc_message_tx
            .send(message)
            .await
            .expect("DocumentActor task has been killed")
    }

    pub fn subscribe_document_changes(&self) -> DocChangedReceiver {
        self.doc_changed_ping_tx.subscribe()
    }

    pub async fn content(&self) -> Result<String> {
        let (send, recv) = oneshot::channel();
        let message = DocMessage::GetContent { response_tx: send };
        // Ignore send errors, because recv.await will fail anyway.
        let _ = self.doc_message_tx.send(message).await;
        recv.await.expect("DocumentActor task has been killed")
    }

    pub async fn apply_random_delta(&mut self) {
        let message = DocMessage::RandomEdit;
        self.doc_message_tx
            .send(message)
            .await
            .expect("Failed to send random edit to document task");
    }

    pub async fn apply_delta(&mut self, delta: TextDelta) {
        let message = DocMessage::Delta(delta);
        self.doc_message_tx
            .send(message)
            .await
            .expect("Failed to send delta to document task");
    }
}

pub struct Daemon {
    pub document_handle: DocumentActorHandle,
}

impl Daemon {
    // Launch the daemon. Optionally, connect to given peer.
    pub fn new(
        port: Option<u16>,
        peer: Option<String>,
        socket_path: &Path,
        file_path: &Path,
    ) -> Self {
        // If the peer address is empty, we're the host.
        let is_host = peer.is_none();

        let document_handle = DocumentActorHandle::new(file_path, is_host);

        let connection_document_handle = document_handle.clone();
        let peer_info = connect::PeerConnectionInfo::new(port, peer);
        tokio::spawn(async move {
            connect::make_peer_connection(peer_info, connection_document_handle).await;
        });

        let editor_socket_path = socket_path.to_path_buf();
        let editor_document_handle = document_handle.clone();
        tokio::spawn(async move {
            connect::make_editor_connection(editor_socket_path, editor_document_handle).await
        });

        Self { document_handle }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod document {
        use super::*;
        use crate::types::factories::*;

        #[test]
        fn can_initialize_content() {
            let mut document = Document::default();
            let text = "To be or not to be, that is the question".to_string();

            document.initialize_text(&text);

            // unfortunately anyhow::Error doesn't implement PartialEq, so we'll rather unwrap.
            assert_eq!(document.current_content().unwrap(), text);
        }

        fn apply_delta_to_doc_works(initial: &str, ed_delta: &EditorTextDelta, expected: &str) {
            let mut document = Document::default();
            document.initialize_text(initial);
            document.apply_delta_to_doc(ed_delta);

            // unfortunately anyhow::Error doesn't implement PartialEq, so we'll rather unwrap.
            assert_eq!(document.current_content().unwrap(), expected);
        }

        #[test]
        fn can_apply_delta_basic_insertion() {
            let ed_delta = ed_delta_single((0, 0), (0, 0), "foobar");
            apply_delta_to_doc_works("", &ed_delta, "foobar");
        }

        #[test]
        fn can_apply_delta_basic_deletion() {
            let ed_delta = ed_delta_single((0, 3), (0, 6), "");
            apply_delta_to_doc_works("foobar", &ed_delta, "foo");
        }

        #[test]
        fn can_apply_delta_basic_replacement() {
            let ed_delta = ed_delta_single((0, 1), (0, 3), "uu");
            apply_delta_to_doc_works("foobar", &ed_delta, "fuubar");
        }

        #[test]
        fn can_apply_delta_multiple_ops() {
            let initial_text = "To be or not to be, that is the question";

            let mut delta = insert(3, "m");
            delta.delete(1); // "b"
            delta.retain(5); // "e or "
            delta.delete(4); // "not "
            delta.retain(3); // "to "
            delta.delete(2); // "be"
            delta.insert("you");

            apply_delta_to_doc_works(
                initial_text,
                &EditorTextDelta::from_delta(delta, initial_text),
                "To me or to you, that is the question",
            );
        }

        #[test]
        fn can_apply_delta_multiple_ops_bug() {
            let content = "xeins\nzwei\ndrei\n";

            let ed_delta = EditorTextDelta(vec![
                replace_ed((1, 0), (1, 0), "xzwei\nx"),
                replace_ed((1, 0), (2, 0), ""),
            ]);

            apply_delta_to_doc_works(content, &ed_delta, "xeins\nxzwei\nxdrei\n");
        }
    }
}
