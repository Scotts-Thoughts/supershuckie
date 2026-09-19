use std::borrow::Cow;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;
use tinyvec::{ArrayVec, TinyVec};
use crate::protocol::{Instruction, MetadataHeader, PokeAByteProtocolRequestPacket, PokeAByteProtocolRequestReadBlock, MAX_NUMBER_OF_READ_BLOCKS};
use crate::shared_memory::PokeAByteSharedMemory;

#[cfg(not(target_pointer_width = "64"))]
compile_error!("must be compiled for 64-bit");

/// The UDP port Poke-A-Byte connects to unless told otherwise (its `PokeAProtocolDriver` default).
///
/// A server on this port shares its memory under the name Poke-A-Byte has always looked for; a
/// server on any other port uses a name derived from the port (see [`shared_memory_name`]), so
/// one machine can run a server per game (the player's own and each friend's in Play Together).
pub const DEFAULT_PORT: u16 = 55356;

pub use crate::shared_memory::shared_memory_name;

/// Command for the emulator to handle.
#[derive(Clone, PartialEq, Debug)]
pub enum PokeAByteEmulatorCommand {
    Reset,
    Write {
        address: u64,
        data: TinyVec<[u8; 16]>,
    },
    Freeze {
        address: u64,
        data: TinyVec<[u8; 16]>,
    },
    Unfreeze {
        address: u64
    }
}

pub struct PokeAByteIntegrationServer {
    session: Arc<Mutex<Option<PokeAByteSession>>>,
    server_close_notifier: Mutex<Receiver<()>>,
    port: u16
}

/// All session-related data from Poke-A-Byte.
pub struct PokeAByteSession {
    /// Shared memory block.
    pub shared_memory: PokeAByteSharedMemory,

    /// Writes requested from Poke-A-Byte.
    pub writes: PokeAByteWriteQueue,

    /// Current setup configuration from the Poke-A-Byte client.
    pub config: PokeAByteSetup,

    address: SocketAddr,
    socket: UdpSocket,
    setup_complete: bool
}

impl PokeAByteSession {
    /// Finish a read.
    ///
    /// This needs to be called on each frame.
    pub fn finish_frame(&mut self) {
        if !self.setup_complete {
            self.setup_complete = true;

            // let Poke-A-Byte know that we're open for business
            let _ = self.socket.send_to(&MetadataHeader::new_response(Instruction::Setup).into_bytes(), &self.address);
        }
    }

    /// Return true if the first frame
    #[inline]
    pub const fn is_first_frame(&self) -> bool {
        !self.setup_complete
    }
}

/// Write queue from Poke-A-Byte.
pub struct PokeAByteWriteQueue {
    queue: Receiver<PokeAByteEmulatorCommand>
}

impl Iterator for PokeAByteWriteQueue {
    type Item = PokeAByteEmulatorCommand;
    fn next(&mut self) -> Option<Self::Item> {
        self.queue.try_recv().ok()
    }
}

/// Configuration shared from Poke-A-Byte.
#[derive(Debug)]
pub struct PokeAByteSetup {
    /// Block mapping.
    ///
    /// This indicates what RAM address in the game corresponds to what offset (and span) in the
    /// shared memory buffer.
    pub blocks: ArrayVec<[PokeAByteProtocolRequestReadBlock; MAX_NUMBER_OF_READ_BLOCKS]>,

    /// Suggested number of frames to skip, if any.
    ///
    /// The emulator can (and ideally should) respect this configuration.
    pub frame_skip: Option<u32>,

    _cant_let_you_instantiate_that_stair_fax: ()
}

impl Drop for PokeAByteIntegrationServer {
    fn drop(&mut self) {
        self.session = Arc::new(Mutex::new(None));
        // The thread notices the session is gone at its next loop iteration, which its socket
        // read (a 500 ms timeout) may otherwise delay; a NoOp to our own port ends the read now,
        // so detaching (or closing a core, which waits for this) does not stall the caller.
        if let Ok(waker) = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)) {
            let _ = waker.send_to(&MetadataHeader::new_request(Instruction::NoOp).into_bytes(), (std::net::Ipv4Addr::LOCALHOST, self.port));
        }
        let _ = self.server_close_notifier.lock().and_then(|i| Ok(i.recv()));
    }
}

impl PokeAByteIntegrationServer {
    /// Begin listening on UDP `127.0.0.1:port` (see [`DEFAULT_PORT`]).
    pub fn begin_listen(port: u16) -> Result<Self, PokeAByteError> {
        if port == 0 {
            return Err(PokeAByteError::SocketFailure { explanation: Cow::Borrowed("port 0 is not a Poke-A-Byte port") })
        }

        let socket = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .map_err(|e| PokeAByteError::SocketFailure { explanation: Cow::Owned(format!("Failed to bind 127.0.0.1:{port}: {e}")) })?;

        let _ = socket.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = socket.set_write_timeout(Some(Duration::from_millis(500)));

        let (sender, receiver) = channel();

        let session = Arc::new(Mutex::new(None));
        let session_downgraded = Arc::downgrade(&session);

        let this = Self {
            session,
            server_close_notifier: Mutex::new(receiver),
            port
        };

        let _ = std::thread::Builder::new().name(format!("PokeAByteIntegrationServer:{port}")).spawn(move || {
            PokeAByteIntegrationServer::thread(session_downgraded, socket, sender, port)
        });

        Ok(this)
    }

    /// The UDP port this server listens on.
    #[inline]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Get the current session, if any.
    pub fn get_session(&self) -> MutexGuard<'_, Option<PokeAByteSession>> {
        self.session.lock().expect("could not get session???")
    }

    fn thread(session: Weak<Mutex<Option<PokeAByteSession>>>, socket: UdpSocket, close_notifier: Sender<()>, port: u16) {
        let mut buffer = vec![0u8; 65536];
        let shared_memory_name = shared_memory_name(port);

        let mut last_setup_user: Option<SocketAddr> = None;
        let mut writer: Option<Sender<PokeAByteEmulatorCommand>> = None;

        loop {
            let Some(promotion) = session.upgrade() else {
                if let Some(addr) = last_setup_user {
                    let _ = socket.send_to(&MetadataHeader::new_response(Instruction::Close).into_bytes(), addr);
                    eprintln!("[PABP:{port}] Disconnecting because server is being terminated/restarted");
                }
                drop(socket);
                let _ = close_notifier.send(());
                return
            };

            let Ok((len, addr)) = socket.recv_from(&mut buffer) else {
                continue
            };

            let bytes_received = &buffer.as_slice()[..len];
            let packet = match PokeAByteProtocolRequestPacket::parse_bytes(bytes_received) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("[PABP:{port}] Error from client @ {addr}: {e:?}");
                    continue
                }
            };

            match packet {
                PokeAByteProtocolRequestPacket::Ping => {
                    let _ = socket.send_to(&MetadataHeader::new_response(Instruction::Ping).into_bytes(), addr);
                },
                PokeAByteProtocolRequestPacket::NoOp => {},
                PokeAByteProtocolRequestPacket::Close => {
                    last_setup_user = None;
                    continue;
                },
                PokeAByteProtocolRequestPacket::Setup { blocks, frame_skip } => {
                    let memory_size = blocks
                        .iter()
                        .map(|i| i.range.end)
                        .max()
                        .unwrap_or(0);

                    let mut session = promotion.lock().expect("Failed to lock: crash?");
                    *session = None; // For cleaning up the old SHM and clearing the file descriptor.

                    // Safety: We're going to zero-initialize this before we use it.
                    let mut shared_memory = match unsafe { PokeAByteSharedMemory::new(&shared_memory_name, memory_size) } {
                        Ok(n) => n,
                        Err(e) => {
                            eprintln!("[PABP:{port}] Failed to instantiate shared memory: {e:?}");
                            continue;
                        }
                    };

                    let (writer_queue, writes_queue) = channel();

                    let writes = PokeAByteWriteQueue { queue: writes_queue };
                    let _ = writer_queue.send(PokeAByteEmulatorCommand::Reset);
                    writer = Some(writer_queue);

                    // note down the address
                    last_setup_user = Some(addr);
                    eprintln!("[PABP:{port}] Accepted new session from client @ {addr} (shared memory {shared_memory_name})");

                    // Zero-initialize
                    unsafe { shared_memory.get_memory_mut() }.fill(0);

                    *session = Some(PokeAByteSession {
                        shared_memory,
                        writes,
                        config: PokeAByteSetup {
                            blocks, frame_skip, _cant_let_you_instantiate_that_stair_fax: ()
                        },
                        address: addr,
                        socket: socket.try_clone().expect("can't clone a UDP socket for some reason"),
                        setup_complete: false
                    });
                },
                PokeAByteProtocolRequestPacket::Write { data, address } => {
                    if Some(addr) != last_setup_user {
                        if last_setup_user.is_none() {
                            eprintln!("[PABP:{port}] Ignoring write from client @ {addr} (no session yet)");
                        }
                        else {
                            eprintln!("[PABP:{port}] Ignoring write from client @ {addr} (address mismatch)");
                        }
                        continue
                    }

                    if data.is_empty() {
                        continue
                    }

                    let Some(writer) = writer.as_ref() else {
                        continue
                    };

                    let _ = writer.send(PokeAByteEmulatorCommand::Write {
                        address, data: data.into()
                    });
                },
                PokeAByteProtocolRequestPacket::Freeze { address, data } => {
                    if Some(addr) != last_setup_user {
                        if last_setup_user.is_none() {
                            eprintln!("[PABP:{port}] Ignoring freeze from client @ {addr} (no session yet)");
                        }
                        else {
                            eprintln!("[PABP:{port}] Ignoring freeze from client @ {addr} (address mismatch)");
                        }
                        continue
                    }

                    if data.is_empty() {
                        continue;
                    }

                    let Some(writer) = writer.as_ref() else {
                        continue
                    };

                    let _ = writer.send(PokeAByteEmulatorCommand::Freeze {
                        address, data: data.into()
                    });
                }
                PokeAByteProtocolRequestPacket::Unfreeze { address } => {
                    if Some(addr) != last_setup_user {
                        if last_setup_user.is_none() {
                            eprintln!("[PABP:{port}] Ignoring freeze from client @ {addr} (no session yet)");
                        }
                        else {
                            eprintln!("[PABP:{port}] Ignoring freeze from client @ {addr} (address mismatch)");
                        }
                        continue
                    }

                    let Some(writer) = writer.as_ref() else {
                        continue
                    };

                    let _ = writer.send(PokeAByteEmulatorCommand::Unfreeze {
                        address
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::METADATA_HEADER_SIZE;

    /// A port nobody is likely to hold; the tests skip themselves if it is taken.
    fn free_port() -> Option<u16> {
        // Bind and release: the port stays free for the moment (loopback, tests run alone).
        let socket = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).ok()?;
        Some(socket.local_addr().ok()?.port())
    }

    #[test]
    fn servers_answer_on_their_own_ports() {
        let (Some(a), Some(b)) = (free_port(), free_port()) else { return };
        let server_a = PokeAByteIntegrationServer::begin_listen(a).expect("bind a");
        let server_b = PokeAByteIntegrationServer::begin_listen(b).expect("bind b");
        assert_eq!(server_a.port(), a);
        assert_eq!(server_b.port(), b);
        // The same port twice is refused, not silently shared.
        assert!(PokeAByteIntegrationServer::begin_listen(a).is_err());
        assert!(PokeAByteIntegrationServer::begin_listen(0).is_err());

        let client = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        for port in [a, b] {
            client.send_to(&MetadataHeader::new_request(Instruction::Ping).into_bytes(), (std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
            let mut reply = [0u8; 64];
            let (len, from) = client.recv_from(&mut reply).expect("a PING reply");
            assert_eq!(from.port(), port, "the reply comes from the port that was pinged");
            assert!(len >= METADATA_HEADER_SIZE);
            assert_eq!(reply[4], Instruction::Ping as u8);
            assert_eq!(reply[5], 1, "it is a response");
        }
    }

    #[test]
    fn dropping_a_server_does_not_wait_for_the_read_timeout() {
        let Some(port) = free_port() else { return };
        let server = PokeAByteIntegrationServer::begin_listen(port).expect("bind");
        let started = std::time::Instant::now();
        drop(server);
        let took = started.elapsed();
        assert!(took < Duration::from_millis(250), "dropping took {took:?} (the socket read timeout is 500 ms)");
        // And the port is free again right away.
        let again = PokeAByteIntegrationServer::begin_listen(port).expect("rebind after drop");
        drop(again);
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum PokeAByteError {
    SharedMemoryFailure { explanation: Cow<'static, str> },
    SocketFailure { explanation: Cow<'static, str> },
    BadPacketFromClient { explanation: Cow<'static, str> }
}

mod shared_memory;
mod protocol;
