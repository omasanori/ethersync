/// A peer is another daemon. This module is all about daemon to daemon communication.
use anyhow::Result;
use automerge::sync::{Message as AutomergeSyncMessage, State as SyncState};
use std::mem;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    sync::{broadcast, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::daemon::{DocMessage, DocumentActorHandle};

// These messages are sent to tasks that own peer sync states.
enum SyncerMessage {
    ReceiveSyncMessage { message: Vec<u8> },
    GenerateSyncMessage,
}

type SyncerMessageSender = mpsc::Sender<SyncerMessage>;
type SyncerMessageReceiver = mpsc::Receiver<SyncerMessage>;

pub fn spawn_peer_sync(stream: TcpStream, document_handle: DocumentActorHandle) {
    let (my_send, my_recv) = oneshot::channel();
    let tcp_handle = TCPActorHandle::start_sync(stream, my_recv);
    let sync_handle = SyncActorHandle::new(document_handle.clone(), tcp_handle);
    let _ = my_send.send(sync_handle);
}

/// Reads from a TCP stream and forwards it to the Syncer
struct TCPReadActor {
    sync_handle: SyncActorHandle,
    reader: ReadHalf<TcpStream>,
    shutdown_token: CancellationToken,
}

impl TCPReadActor {
    fn new(
        reader: ReadHalf<TcpStream>,
        sync_handle: SyncActorHandle,
        shutdown_token: CancellationToken,
    ) -> Self {
        Self {
            sync_handle,
            reader,
            shutdown_token,
        }
    }

    async fn forward_sync_message(&self, message: Vec<u8>) {
        self.sync_handle
            .send(SyncerMessage::ReceiveSyncMessage { message })
            .await
    }

    async fn read_message(&mut self) -> Result<Vec<u8>> {
        let mut message_len_buf = [0; 4];
        self.reader.read_exact(&mut message_len_buf).await?;
        let message_len = i32::from_be_bytes(message_len_buf);
        let mut message_buf = vec![0; message_len as usize];
        self.reader.read_exact(&mut message_buf).await?;
        Ok(message_buf)
    }

    async fn run(&mut self) {
        while let Ok(message) = self.read_message().await {
            self.forward_sync_message(message).await;
        }
        info!("Sync Receive loop stopped (peer disconnected)");
        self.shutdown_token.cancel()
    }
}

struct TCPWriteActor {
    writer: WriteHalf<TcpStream>,
    automerge_message_receiver: mpsc::Receiver<AutomergeSyncMessage>,
}

impl TCPWriteActor {
    fn new(
        writer: WriteHalf<TcpStream>,
        automerge_message_receiver: mpsc::Receiver<AutomergeSyncMessage>,
    ) -> Self {
        Self {
            writer,
            automerge_message_receiver,
        }
    }

    async fn run(&mut self) {
        while let Some(message) = self.automerge_message_receiver.recv().await {
            // TODO: move encode to Syncer for symmetry?
            let message = message.encode();
            let message_len = message.len() as i32;
            self.writer
                .write_all(&message_len.to_be_bytes())
                .await
                .expect("GenerateSyncMessage: write message len failed");
            self.writer
                .write_all(&message)
                .await
                .expect("GenerateSyncMessage: write message failed");
        }
        // At this point, our channel has been closed, which is the signal for us to stop.
        debug!("TCPWriteActor stopped (channel closed)");
    }
}

struct SyncActor {
    syncer_receiver: SyncerMessageReceiver,
    document_handle: DocumentActorHandle,
    tcp_handle: TCPActorHandle,
    peer_state: SyncState,
}

impl SyncActor {
    fn new(
        syncer_receiver: SyncerMessageReceiver,
        document_handle: DocumentActorHandle,
        tcp_handle: TCPActorHandle,
    ) -> Self {
        Self {
            syncer_receiver,
            document_handle,
            tcp_handle,
            peer_state: SyncState::new(),
        }
    }

    async fn handle_message(&mut self, message: SyncerMessage) {
        match message {
            SyncerMessage::ReceiveSyncMessage { message } => {
                let (reponse_tx, response_rx) = oneshot::channel();
                let message = AutomergeSyncMessage::decode(&message)
                    .expect("Failed to decode automerge message");
                self.document_handle
                    .send_message(DocMessage::ReceiveSyncMessage {
                        message,
                        state: mem::take(&mut self.peer_state),
                        response_tx: reponse_tx,
                    })
                    .await;
                self.peer_state = response_rx
                    .await
                    .expect("Couldn't read response from Document channel");
            }
            SyncerMessage::GenerateSyncMessage {} => {
                let (reponse_tx, response_rx) = oneshot::channel();
                self.document_handle
                    .send_message(DocMessage::GenerateSyncMessage {
                        state: mem::take(&mut self.peer_state),
                        response_tx: reponse_tx,
                    })
                    .await;
                let (ps, message) = response_rx
                    .await
                    .expect("Could not read response from Document channel");
                self.peer_state = ps;
                if let Some(message) = message {
                    self.tcp_handle.send(message).await;
                }
            }
        }
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                _ = self.tcp_handle.shutdown_token.cancelled() => {
                    debug!("Shutting down main start_sync loop");
                    break;
                }
                // TODO: Also listen to document changed pings here, and react directly, instead of
                // having the loop in SyncActorHandle::new forward it to us.
                Some(message) = self.syncer_receiver.recv() => {
                    self.handle_message(message).await;
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct SyncActorHandle {
    syncer_message_tx: SyncerMessageSender,
}

impl SyncActorHandle {
    pub fn new(document_handle: DocumentActorHandle, tcp_handle: TCPActorHandle) -> Self {
        let (syncer_message_tx, syncer_message_rx) = mpsc::channel(16);

        // Sync actor.
        let syncer = SyncActor::new(
            syncer_message_rx,
            document_handle.clone(),
            tcp_handle.clone(),
        );
        tokio::spawn(syncer.run());

        // Generate sync message when doc changes.
        let shutdown_token_clone = tcp_handle.shutdown_token.clone();
        let mut doc_changed_ping_rx = document_handle.subscribe_document_changes();
        let syncer_message_tx_clone = syncer_message_tx.clone();

        // TODO: can we explain here, why this forwarding is necessary?
        tokio::spawn(async move {
            loop {
                syncer_message_tx_clone
                    .send(SyncerMessage::GenerateSyncMessage {})
                    .await
                    .expect("Failed to send GenerateSyncMessage to document task");
                tokio::select! {
                    _ = shutdown_token_clone.cancelled() => {
                        debug!("Stopping GenerateSyncMessage ping forwarding.");
                        break;
                    }
                    doc_ping = doc_changed_ping_rx.recv() => match doc_ping {
                        Ok(()) => {
                            debug!("Doc changed.");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            panic!("Doc changed channel has been closed");
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // This is fine, the messages in this channel are just pings.
                            // It's okay if we miss some.
                        }
                    }
                }
            }
        });

        Self { syncer_message_tx }
    }

    async fn send(&self, message: SyncerMessage) {
        self.syncer_message_tx
            .send(message)
            .await
            .expect("Channel closed (TODO)")
    }
}

#[derive(Clone)]
pub struct TCPActorHandle {
    automerge_message_tx: mpsc::Sender<AutomergeSyncMessage>,
    shutdown_token: CancellationToken,
}

/// The TCP statemachine works as follows:
/// - if we're the host,
/// - if we're a peer, we
///
/// How do other parts of the code communicate with TCP? Through this handle.
/// What can be communicated?
impl TCPActorHandle {
    async fn send(&mut self, message: AutomergeSyncMessage) {
        self.automerge_message_tx
            .send(message)
            .await
            .expect("Channel to TCPActor(s) closed.");
    }

    pub fn start_sync(stream: TcpStream, sync_handle: oneshot::Receiver<SyncActorHandle>) -> Self {
        let shutdown_token = CancellationToken::new();

        let read_shutdown_token = shutdown_token.clone();
        let (tcp_read, tcp_write) = tokio::io::split(stream);
        let (automerge_message_tx, automerge_message_rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let sync_handle = match sync_handle.await {
                Ok(my_handle) => my_handle,
                Err(_) => return,
            };
            let mut receiver = TCPReadActor::new(tcp_read, sync_handle, read_shutdown_token);
            tokio::spawn(async move {
                receiver.run().await;
            });
            let mut writer = TCPWriteActor::new(tcp_write, automerge_message_rx);
            tokio::spawn(async move {
                writer.run().await;
            });
        });
        Self {
            automerge_message_tx,
            shutdown_token,
        }
    }
}
