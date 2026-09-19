use std::collections::BTreeMap;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::*;
use std::time::{Duration, Instant};
use rouille::{Response, Server};
use serde::{Deserialize, Deserializer, Serialize};

/// How long a client waits for a reply before it gives up and gets a 503/timeout error. A queued
/// command still sitting in the backlog at or beyond this age has already been answered that way,
/// so `next_server_command` skips it instead of executing it late.
const REPLY_TIMEOUT: Duration = Duration::from_secs(60);

/// How many bind attempts `SuperShuckieWebserver::new` makes before giving up: the first attempt
/// plus this many retries, 50 ms apart, to ride out tiny_http's asynchronous listener teardown
/// after a disable immediately followed by an enable.
const BIND_RETRIES: u32 = 10;

pub struct SuperShuckieWebserver {
    backlog: Receiver<(Instant, SuperShuckieServerCommand)>,
    should_continue: Arc<AtomicBool>
}

#[derive(Serialize)]
struct Error {
    error: String
}

impl SuperShuckieWebserver {
    /// Instantiate the server.
    pub fn new<S: ToSocketAddrs>(addr: S) -> Result<Self, String> {
        // Resolved once so every bind attempt below targets the same address without requiring
        // `S` itself to be `Clone`.
        let addrs: Vec<std::net::SocketAddr> = addr.to_socket_addrs()
            .map_err(|e| format!("Failed to make SuperShuckieServer:\n\n{e}"))?
            .collect();

        let (backlog_sender, backlog_receiver) = sync_channel(64);
        let should_continue = Arc::new(AtomicBool::new(true));
        let emulator_not_available_error = || {
            fixup_response(Response::json(&Error {
                error: "failed (emulator is not available)".to_owned()
            }).with_status_code(503))
        };

        // Built once so a bind failure (see below) can retry with the exact same handler.
        let handler = move |request: &rouille::Request| {
            let url = request.url();

            if let Some(route) = BookmarkRoute::from_path(url.as_str()) {
                let bookmark_request = match route.parse(request) {
                    Ok(n) => n,
                    Err(error) => return fixup_response(Response::json(&Error { error }).with_status_code(400))
                };

                let (sender, response) = channel();
                if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::Bookmarks(sender, bookmark_request))).is_err() {
                    return emulator_not_available_error();
                }

                return fixup_response(match response.recv_timeout(REPLY_TIMEOUT) {
                    Ok(Ok(Some(json))) => Response::from_data("application/json", json),
                    Ok(Ok(None)) => Response::empty_204(),
                    Ok(Err((status, error))) => Response::json(&Error { error }).with_status_code(status),
                    Err(_) => return emulator_not_available_error()
                })
            }

            fixup_response(match url.as_str() {
                "/stats" => {
                    let (responder, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::Stats(responder))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(n) => Response::json(Arc::as_ref(&n)),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/play-together" => {
                    let (responder, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::PlayTogetherState(responder))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(json) => Response::from_data("application/json", json),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/mark-start" => {
                    let (responder, response) = channel();

                    let offset = match request.get_param("offset") {
                        None => 0,
                        Some(n) => match n.parse() {
                            Ok(n) => n,
                            Err(_) => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an unsigned integer)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::MarkStart(responder, offset))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not recording a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/mark-end" => {
                    let (responder, response) = channel();
                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::MarkEnd(responder))).is_err() {
                        return emulator_not_available_error()
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not recording a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/increment-counter" => {
                    let Some(name) = request.get_param("name") else {
                        return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the name parameter)".to_owned()
                            }).with_status_code(400)
                        )
                    };

                    let by = match request.get_param("by") {
                        None => 1,
                        Some(n) => match n.parse() {
                            Ok(n) => n,
                            Err(_) => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an integer)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::IncrementCounter(sender, name, by))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not recording a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/enumerate-replays" => {
                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::EnumerateReplays(sender))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(n) => Response::json(n.as_ref()),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/set-playback-speed" => {
                    let speed = match request.get_param("speed") {
                        None => return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the speed parameter)".to_owned()
                            }).with_status_code(400)
                        ),
                        Some(n) => match n.parse::<f64>() {
                            Ok(n) if n.is_finite() && n > 0.0 => n,
                            _ => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an float)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::SetPlaybackSpeed(sender, speed))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not playing a game)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/load-replay" => {
                    let Some(name) = request.get_param("name") else {
                        return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the name parameter)".to_owned()
                            }).with_status_code(400)
                        )
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::LoadReplay(sender, name))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (replay probably not found or is invalid)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/set-paused" => {
                    let paused = match request.get_param("paused") {
                        None => return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the paused parameter)".to_owned()
                            }).with_status_code(400)
                        ),
                        Some(n) => match n.parse() {
                            Ok(paused) => paused,
                            Err(_) => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an integer)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::SetPaused(sender, paused))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (unknown reason)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/go-to-frame" => {
                    let frame = match request.get_param("frame") {
                        None => return fixup_response(
                            Response::json(&Error {
                                error: "failed (missing the frame parameter)".to_owned()
                            }).with_status_code(400)
                        ),
                        Some(n) => match n.parse() {
                            Ok(n) => n,
                            Err(_) => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an integer)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::GoToFrame(sender, frame))).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not playing back a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/load-rom" => {
                    let Some(path) = request.get_param("path") else {
                        return fixup_response(Response::json(&Error {
                            error: "failed (missing the path parameter)".to_owned()
                        }).with_status_code(400));
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send((Instant::now(), SuperShuckieServerCommand::LoadROM(sender, path.into()))).is_err() {
                        return emulator_not_available_error();
                    };

                    match response.recv_timeout(REPLY_TIMEOUT) {
                        Ok(Ok(_)) => Response::empty_204(),
                        Ok(Err(error)) => Response::json(&Error { error }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/client.js" => {
                    Response::text(include_str!("../js/client.js"))
                        .with_unique_header(
                            "Content-Type",
                            "text/javascript; charset=utf-8"
                        )
                }
                _ => Response::empty_404()
            })
        };

        // Disabling the server drops it, but tiny_http closes its listening socket
        // asynchronously; an immediate re-enable can otherwise race that teardown and fail to
        // bind. Retry a few times, 50 ms apart, before giving up with the bind error.
        let mut bind_result = Server::new(addrs.as_slice(), handler.clone()).map(|s| s.pool_size(4));
        for _ in 0..BIND_RETRIES {
            if bind_result.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            bind_result = Server::new(addrs.as_slice(), handler.clone()).map(|s| s.pool_size(4));
        }
        let server = bind_result.map_err(|e| format!("Failed to make SuperShuckieServer:\n\n{e}"))?;

        let should_continue_inner = should_continue.clone();

        std::thread::spawn(move || {
            while should_continue_inner.load(Ordering::Relaxed) {
                server.poll();
                std::thread::sleep(Duration::from_millis(1));
            }
        });

        Ok(Self {
            backlog: backlog_receiver,
            should_continue
        })
    }

    /// Get the next server command.
    ///
    /// A command issued long enough ago that its client already gave up and received a
    /// 503/timeout error (see `REPLY_TIMEOUT`) is skipped rather than executed late; its reply
    /// channel is dropped along with it, which is harmless since nothing is listening any more.
    #[inline]
    pub fn next_server_command(&mut self) -> Option<SuperShuckieServerCommand> {
        loop {
            let (issued, command) = self.backlog.try_recv().ok()?;
            if issued.elapsed() < REPLY_TIMEOUT {
                return Some(command);
            }
        }
    }
}

fn fixup_response(response: Response) -> Response {
    response.with_unique_header("Access-Control-Allow-Origin", "*")
}

impl Drop for SuperShuckieWebserver {
    fn drop(&mut self) {
        self.should_continue.store(false, Ordering::Relaxed);

        // clear the backlog
        while self.backlog.recv().is_ok() {}
    }
}

/// The bookmark routes.
#[derive(Copy, Clone, PartialEq, Debug)]
enum BookmarkRoute {
    List,
    Add,
    Update,
    Delete,
    ToggleRange,
    GoTo
}

impl BookmarkRoute {
    fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            "/bookmarks" => Self::List,
            "/add-bookmark" => Self::Add,
            "/update-bookmark" => Self::Update,
            "/delete-bookmark" => Self::Delete,
            "/toggle-range-bookmark" => Self::ToggleRange,
            "/go-to-bookmark" => Self::GoTo,
            _ => return None
        })
    }

    fn parse(self, request: &rouille::Request) -> Result<BookmarkRequest, String> {
        parse_bookmark_request(self, &|name| request.get_param(name))
    }
}

fn parse_bookmark_request(route: BookmarkRoute, param: &dyn Fn(&str) -> Option<String>) -> Result<BookmarkRequest, String> {
    let number = |name: &str| -> Result<Option<u64>, String> {
        match param(name) {
            None => Ok(None),
            Some(n) => n.trim().parse().map(Some).map_err(|_| format!("failed (can't parse {n} as an unsigned integer for {name})"))
        }
    };
    let id = || -> Result<u64, String> {
        number("id")?.ok_or_else(|| "failed (missing the id parameter)".to_owned())
    };
    let params = || -> Result<BookmarkParams, String> {
        Ok(BookmarkParams {
            name: param("name"),
            type_name: param("type"),
            type_id: param("type_id"),
            frame: number("frame")?,
            out: param("out"),
            keyframe: match param("keyframe") {
                None => None,
                Some(n) => Some(n.trim().parse().map_err(|_| format!("failed (can't parse {n} as true or false for keyframe)"))?)
            }
        })
    };

    Ok(match route {
        BookmarkRoute::List => BookmarkRequest::List,
        BookmarkRoute::Add => BookmarkRequest::Add(params()?),
        BookmarkRoute::Update => BookmarkRequest::Update(id()?, params()?),
        BookmarkRoute::Delete => BookmarkRequest::Delete(id()?),
        BookmarkRoute::ToggleRange => BookmarkRequest::ToggleRange(params()?),
        BookmarkRoute::GoTo => BookmarkRequest::GoTo(id()?, match param("point").as_deref().map(str::trim) {
            None | Some("in") => false,
            Some("out") => true,
            Some(other) => return Err(format!("failed (point must be in or out, not {other})"))
        })
    })
}

/// A bookmark route's outcome: a JSON body (200), no body (204), or an HTTP status and message.
pub type BookmarkReply = Result<Option<String>, (u16, String)>;

/// What a bookmark route asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum BookmarkRequest {
    /// `/bookmarks`
    List,
    /// `/add-bookmark`
    Add(BookmarkParams),
    /// `/update-bookmark` (id)
    Update(u64, BookmarkParams),
    /// `/delete-bookmark` (id)
    Delete(u64),
    /// `/toggle-range-bookmark`
    ToggleRange(BookmarkParams),
    /// `/go-to-bookmark` (id, whether to go to the out frame)
    GoTo(u64, bool)
}

/// Optional parameters of the bookmark routes, as given (also accepted as JSON by the C API).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct BookmarkParams {
    pub name: Option<String>,
    /// `type`: a type name; `none` for untyped.
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// `type_id`: a type id (hex).
    pub type_id: Option<String>,
    pub frame: Option<u64>,
    /// `out`: `now`, `none`, or a frame.
    #[serde(deserialize_with = "string_or_number")]
    pub out: Option<String>,
    pub keyframe: Option<bool>
}

fn string_or_number<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Value {
        Text(String),
        Number(u64)
    }

    Ok(Option::<Value>::deserialize(deserializer)?.map(|v| match v {
        Value::Text(text) => text,
        Value::Number(n) => n.to_string()
    }))
}

pub enum SuperShuckieServerCommand {
    Stats(Sender<Arc<Stats>>),
    /// `/play-together`: the Play Together state as JSON (the same document the C API's
    /// `supershuckie_frontend_play_together_state_json` gives).
    PlayTogetherState(Sender<String>),
    Bookmarks(Sender<BookmarkReply>, BookmarkRequest),
    MarkStart(Sender<bool>, u32),
    MarkEnd(Sender<bool>),
    IncrementCounter(Sender<bool>, String, i64),
    GoToFrame(Sender<bool>, u32),
    SetPaused(Sender<bool>, bool),
    LoadReplay(Sender<bool>, String),
    EnumerateReplays(Sender<Arc<Vec<String>>>),
    SetPlaybackSpeed(Sender<bool>, f64),
    LoadROM(Sender<Result<(), String>>, PathBuf)
}

#[derive(Clone, Serialize)]
pub struct Stats {
    pub time_start: Option<u32>,
    pub time_end: Option<u32>,
    pub time_offset: Option<u32>,
    pub time_current: Option<u32>,

    pub total_elapsed_time: u32,
    pub total_elapsed_frames: u32,

    pub is_recording: bool,
    /// A replay is loaded for playback (playing, or stopped: see `is_playback_stopped`).
    pub is_playing_back: bool,
    /// The loaded replay is stopped: still loaded (seekable, resumable) but the game is running
    /// live under the user's control.
    pub is_playback_stopped: bool,
    pub is_paused: bool,
    pub is_playback_finished: bool,

    pub counters: BTreeMap<String, i64>,

    pub current_speed: f64,

    /// Emulated frames per second over the last second (drawn or not), i.e. the real emulation
    /// rate; the on-screen refresh rate is at most 60.
    pub emulation_fps: f64,

    /// Average time the core spent on one frame recently, in milliseconds.
    pub frame_time_ms: f64,

    /// Time one frame may take at the current speed, in milliseconds (0 if the core does not
    /// pace itself).
    pub frame_budget_ms: f64,

    /// Frames that took longer than the budget since the last speed change or ROM load.
    pub frames_over_budget: u64,

    /// Changes whenever the current replay's bookmarks (or the bookmark types) change; fetch
    /// `/bookmarks` when it does.
    pub bookmark_generation: u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(route: &str, query: &[(&str, &str)]) -> Result<BookmarkRequest, String> {
        let lookup = |name: &str| query.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string());
        parse_bookmark_request(BookmarkRoute::from_path(route).expect("route"), &lookup)
    }

    #[test]
    fn bookmark_routes_parse_their_parameters() {
        assert_eq!(parse("/bookmarks", &[]), Ok(BookmarkRequest::List));
        assert_eq!(
            parse("/add-bookmark", &[("name", "Death"), ("type", "deaths"), ("frame", "1200"), ("out", "now"), ("keyframe", "false")]),
            Ok(BookmarkRequest::Add(BookmarkParams { name: Some("Death".into()), type_name: Some("deaths".into()), type_id: None, frame: Some(1200), out: Some("now".into()), keyframe: Some(false) }))
        );
        assert_eq!(parse("/update-bookmark", &[("id", "7"), ("out", "none")]), Ok(BookmarkRequest::Update(7, BookmarkParams { out: Some("none".into()), ..Default::default() })));
        assert_eq!(parse("/delete-bookmark", &[("id", "3")]), Ok(BookmarkRequest::Delete(3)));
        assert_eq!(parse("/toggle-range-bookmark", &[("type_id", "00000000000000ab")]), Ok(BookmarkRequest::ToggleRange(BookmarkParams { type_id: Some("00000000000000ab".into()), ..Default::default() })));
        assert_eq!(parse("/go-to-bookmark", &[("id", "3")]), Ok(BookmarkRequest::GoTo(3, false)));
        assert_eq!(parse("/go-to-bookmark", &[("id", "3"), ("point", "out")]), Ok(BookmarkRequest::GoTo(3, true)));
        assert!(BookmarkRoute::from_path("/bookmark").is_none());
    }

    #[test]
    fn bad_bookmark_parameters_are_rejected() {
        assert!(parse("/update-bookmark", &[]).unwrap_err().contains("missing the id"));
        assert!(parse("/delete-bookmark", &[("id", "-1")]).unwrap_err().contains("unsigned integer"));
        assert!(parse("/add-bookmark", &[("frame", "soon")]).unwrap_err().contains("frame"));
        assert!(parse("/add-bookmark", &[("keyframe", "yes")]).unwrap_err().contains("keyframe"));
        assert!(parse("/go-to-bookmark", &[("id", "1"), ("point", "middle")]).unwrap_err().contains("in or out"));
    }

    /// Builds a `SuperShuckieWebserver` directly on top of a fresh backlog channel, bypassing
    /// `new` (and its HTTP listener) entirely, so `next_server_command`'s age-based skipping can
    /// be tested without a real client.
    fn webserver_with_backlog() -> (SuperShuckieWebserver, SyncSender<(Instant, SuperShuckieServerCommand)>) {
        let (sender, backlog) = sync_channel(4);
        (SuperShuckieWebserver { backlog, should_continue: Arc::new(AtomicBool::new(true)) }, sender)
    }

    #[test]
    fn stale_commands_are_skipped_but_fresh_ones_are_returned() {
        let (mut server, sender) = webserver_with_backlog();

        let (stale_responder, _stale_response) = channel();
        sender.send((Instant::now() - REPLY_TIMEOUT - Duration::from_secs(1), SuperShuckieServerCommand::Stats(stale_responder))).unwrap();

        let (fresh_responder, _fresh_response) = channel();
        sender.send((Instant::now(), SuperShuckieServerCommand::Stats(fresh_responder))).unwrap();

        // Drop the sender now: the messages above are already buffered, but the test would
        // otherwise hang in `Drop::drop`'s drain loop, which waits for the channel to disconnect.
        drop(sender);

        assert!(matches!(server.next_server_command(), Some(SuperShuckieServerCommand::Stats(_))), "the fresh command should still come back");
        assert!(server.next_server_command().is_none(), "the stale command should have been skipped, and nothing else is queued");
    }
}
