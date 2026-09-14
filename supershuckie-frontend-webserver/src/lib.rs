use std::collections::BTreeMap;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::*;
use std::time::Duration;
use rouille::{Response, Server};
use serde::{Deserialize, Deserializer, Serialize};

pub struct SuperShuckieWebserver {
    backlog: Receiver<SuperShuckieServerCommand>,
    should_continue: Arc<AtomicBool>
}

#[derive(Serialize)]
struct Error {
    error: String
}

impl SuperShuckieWebserver {
    /// Instantiate the server.
    pub fn new<S: ToSocketAddrs>(addr: S) -> Result<Self, String> {
        let (backlog_sender, backlog_receiver) = sync_channel(1024);
        let should_continue = Arc::new(AtomicBool::new(true));
        let emulator_not_available_error = || {
            fixup_response(Response::json(&Error {
                error: "failed (emulator is not available)".to_owned()
            }).with_status_code(503))
        };

        let server = Server::new(addr, move |request| {
            let url = request.url();

            if let Some(route) = BookmarkRoute::from_path(url.as_str()) {
                let bookmark_request = match route.parse(request) {
                    Ok(n) => n,
                    Err(error) => return fixup_response(Response::json(&Error { error }).with_status_code(400))
                };

                let (sender, response) = channel();
                if backlog_sender.try_send(SuperShuckieServerCommand::Bookmarks(sender, bookmark_request)).is_err() {
                    return emulator_not_available_error();
                }

                return fixup_response(match response.recv_timeout(Duration::from_secs(60)) {
                    Ok(Ok(Some(json))) => Response::from_data("application/json", json),
                    Ok(Ok(None)) => Response::empty_204(),
                    Ok(Err((status, error))) => Response::json(&Error { error }).with_status_code(status),
                    Err(_) => return emulator_not_available_error()
                })
            }

            fixup_response(match url.as_str() {
                "/stats" => {
                    let (responder, response) = channel();

                    if backlog_sender.try_send(SuperShuckieServerCommand::Stats(responder)).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
                        Ok(n) => Response::json(Arc::as_ref(&n)),
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

                    if backlog_sender.try_send(SuperShuckieServerCommand::MarkStart(responder, offset)).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not recording a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                },
                "/mark-end" => {
                    let (responder, response) = channel();
                    if backlog_sender.try_send(SuperShuckieServerCommand::MarkEnd(responder)).is_err() {
                        return emulator_not_available_error()
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
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

                    if backlog_sender.try_send(SuperShuckieServerCommand::IncrementCounter(sender, name, by)).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not recording a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/enumerate-replays" => {
                    let (sender, response) = channel();

                    if backlog_sender.try_send(SuperShuckieServerCommand::EnumerateReplays(sender)).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
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
                            Ok(n) if n.is_finite() && n.is_sign_positive() => n,
                            _ => return fixup_response(
                                Response::json(&Error {
                                    error: format!("failed (can't parse {n} as an float)")
                                }).with_status_code(400)
                            )
                        }
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send(SuperShuckieServerCommand::SetPlaybackSpeed(sender, speed)).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
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

                    if backlog_sender.try_send(SuperShuckieServerCommand::LoadReplay(sender, name)).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
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
                                error: "failed (missing the frame parameter)".to_owned()
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

                    if backlog_sender.try_send(SuperShuckieServerCommand::SetPaused(sender, paused)).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
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

                    if backlog_sender.try_send(SuperShuckieServerCommand::GoToFrame(sender, frame)).is_err() {
                        return emulator_not_available_error();
                    }

                    match response.recv_timeout(Duration::from_secs(60)) {
                        Ok(true) => Response::empty_204(),
                        Ok(false) => Response::json(&Error {
                            error: "error (probably not playing back a replay)".to_owned()
                        }).with_status_code(404),
                        Err(_) => return emulator_not_available_error()
                    }
                }
                "/load-rom" => {
                    let Some(path) = request.get_param("path") else {
                        return Response::json(&Error {
                            error: "failed (missing the path parameter)".to_owned()
                        }).with_status_code(400);
                    };

                    let (sender, response) = channel();

                    if backlog_sender.try_send(SuperShuckieServerCommand::LoadROM(sender, path.into())).is_err() {
                        return emulator_not_available_error();
                    };

                    match response.recv_timeout(Duration::from_secs(60)) {
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
                _ => Response::empty_400()
            })
        }).map_err(|e| format!("Failed to make SuperShuckieServer:\n\n{e}"))?;

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
    #[inline]
    pub fn next_server_command(&mut self) -> Option<SuperShuckieServerCommand> {
        self.backlog.try_recv().ok()
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
    pub is_playing_back: bool,
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
}
