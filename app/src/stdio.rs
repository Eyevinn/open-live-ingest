//! `--ipc-stdio`: the app core with no window and no webview, driven over stdin and
//! stdout. It is how the agent-run end to end (OL-59i, docs/specs/desktop-app.md
//! section 14) drives the app from the release artifacts: the same executable the DMG
//! or the installer carries, the same commands the page sends, through the same
//! [`Core`].
//!
//! Each line on stdin is one message the page would post (`{"id": <u32>, "cmd":
//! "<name>", "args": {...}}`, section 9.1). Each line on stdout is one JSON value the
//! page would be handed through `window.oli.receive`: a reply, or an event. Nothing
//! else is written to stdout; the log goes to `app.log` as always. Replies and events
//! cannot carry a secret ([`crate::ipc::Secret`] has no `Serialize`), so stdout can
//! be logged.
//!
//! The process ends as the window does: when the core quits (a `quit` command with
//! nothing live, or once Stop and quit is done), on SIGTERM (Stop without asking, as
//! `open-live-ingest down` does to the window), or when stdin closes. A closed stdin
//! while live is treated like the session ending from outside: the bounded teardown
//! runs ([`QuitHandle::end_session`]).

use crate::ipc::{Core, Deliver, Effect};
use crate::quit::{Exit, QuitHandle};
use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

/// The flag that starts the app this way.
pub const FLAG: &str = "--ipc-stdio";

/// Whether the command line asks for the core over stdin and stdout.
pub fn wanted<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    args.into_iter().any(|a| a.as_ref() == FLAG)
}

/// The JSON a reply or event script hands to the page: the argument of
/// `window.oli.receive(...)`.
pub fn payload(script: &str) -> &str {
    script
        .strip_prefix("window.oli.receive(")
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(script)
}

/// Where the replies and events go: stdout, or a buffer in tests.
pub type Out = Arc<Mutex<dyn Write + Send>>;

enum Next {
    Line(String),
    /// stdin closed, or could not be read.
    End,
    /// The core says the app ends now.
    Exit,
}

fn write_line(out: &Out, json: &str) {
    let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
    // A reader that went away cannot be told anything; the session still ends as the
    // core decides.
    let _ = writeln!(out, "{json}").and_then(|()| out.flush());
}

/// The app core over the default settings file, on this process's stdin and stdout.
pub fn run() -> ExitCode {
    let settings = open_live_ingest::config::default_path();
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    serve(
        move |deliver| Core::new(settings, deliver),
        std::io::BufReader::new(std::io::stdin()),
        out,
    )
}

/// Reads messages from `input` and hands them to the core `make_core` builds, writing
/// every reply and event to `out`, until the core quits or `input` ends.
pub fn serve<R>(make_core: impl FnOnce(Deliver) -> Core, input: R, out: Out) -> ExitCode
where
    R: BufRead + Send + 'static,
{
    let (tx, rx) = channel::<Next>();
    let deliver: Deliver = {
        let out = out.clone();
        Arc::new(move |script: String| write_line(&out, payload(&script)))
    };
    let exit: Exit = {
        let tx = Mutex::new(tx.clone());
        Arc::new(move || {
            let _ = tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send(Next::Exit);
        })
    };
    let mut core = make_core(deliver).with_exit(exit);
    let quitting = core.quit_handle();
    #[cfg(unix)]
    quitting.stop_on_sigterm();
    read_lines(input, tx);

    tracing::info!(
        version = crate::VERSION,
        "app core started on stdin and stdout"
    );
    while let Ok(next) = rx.recv() {
        match next {
            Next::Line(line) if line.trim().is_empty() => {}
            Next::Line(line) => {
                if let Some((reply, effect)) = core.dispatch(&line) {
                    write_line(&out, payload(&reply.script()));
                    if effect == Effect::Quit {
                        break;
                    }
                }
            }
            Next::End => {
                tracing::info!("stdin closed");
                break;
            }
            Next::Exit => break,
        }
    }
    end(&quitting);
    ExitCode::SUCCESS
}

/// As the window's `LoopDestroyed`: nothing left running unbounded.
fn end(quitting: &QuitHandle) {
    quitting.end_session();
    tracing::info!("quitting");
}

fn read_lines<R: BufRead + Send + 'static>(input: R, tx: Sender<Next>) {
    std::thread::Builder::new()
        .name("oli-stdin".into())
        .spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else { break };
                if tx.send(Next::Line(line)).is_err() {
                    return;
                }
            }
            let _ = tx.send(Next::End);
        })
        .expect("starting the stdin reader");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use crate::where_to_send::WhereToSend;
    use open_live_ingest::keychain::MemoryKeychain;
    use open_live_ingest::osc;
    use serde_json::{json, Value};
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::time::Duration;

    /// A buffer the test can read after `serve` has written to it.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lines(buffer: &Buffer) -> Vec<Value> {
        String::from_utf8(buffer.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line is JSON"))
            .collect()
    }

    /// Serves `input` to a core over `settings` with no Open Source Cloud reachable.
    fn serve_input(settings: PathBuf, input: &str) -> Vec<Value> {
        let buffer = Buffer::default();
        let out: Out = Arc::new(Mutex::new(buffer.clone()));
        let make = move |deliver: Deliver| {
            let where_to_send = WhereToSend::new(
                settings.clone(),
                Arc::new(MemoryKeychain::new()),
                osc::Endpoints {
                    connect: "http://127.0.0.1:9".into(),
                    catalog: "http://127.0.0.1:9/mysubscriptions".into(),
                    token_exchange: "http://127.0.0.1:9/servicetoken".into(),
                },
                Duration::from_secs(10),
                testing::browser(testing::NEVER),
            );
            Core::with_where_to_send(settings, where_to_send, deliver)
        };
        assert_eq!(
            serve(make, Cursor::new(input.to_string()), out),
            ExitCode::SUCCESS
        );
        lines(&buffer)
    }

    #[test]
    fn only_the_flag_asks_for_it() {
        assert!(wanted(["--ipc-stdio"]));
        assert!(wanted(["-psn_0_1", "--ipc-stdio"]));
        assert!(!wanted(Vec::<String>::new()));
        assert!(!wanted(["--ipc", "ipc-stdio", "--version"]));
    }

    #[test]
    fn the_payload_is_the_json_the_page_is_handed() {
        assert_eq!(
            payload(r#"window.oli.receive({"id":1,"ok":null})"#),
            r#"{"id":1,"ok":null}"#
        );
        assert_eq!(payload("{}"), "{}");
    }

    /// Each line in is a page message; each line out is the reply the page would get,
    /// in order, and the process ends when stdin closes.
    #[test]
    fn replies_go_out_one_json_line_each_until_stdin_ends() {
        let dir = testing::Scratch::new("stdio-replies");
        let got = serve_input(
            dir.settings(),
            "{\"id\": 1, \"cmd\": \"state\"}\n\n{\"id\": 2, \"cmd\": \"no_such_command\"}\nnot json\n{\"id\": 3, \"cmd\": \"stop\"}\n",
        );
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0]["id"], 1);
        assert_eq!(got[0]["ok"]["live"], false);
        assert_eq!(
            got[0]["ok"]["settings_file"],
            dir.settings().display().to_string()
        );
        assert_eq!(got[1]["id"], 2);
        assert!(got[1]["error"]["message"].is_string());
        assert_eq!(got[2], json!({"id": 3, "ok": null}));
    }

    /// `quit` with nothing live ends the process after its reply, as it closes the
    /// window: what follows on stdin is not read.
    #[test]
    fn quit_with_nothing_live_ends_after_its_reply() {
        let dir = testing::Scratch::new("stdio-quit");
        let got = serve_input(
            dir.settings(),
            "{\"id\": 1, \"cmd\": \"quit\", \"args\": {\"confirmed\": false}}\n{\"id\": 2, \"cmd\": \"state\"}\n",
        );
        assert_eq!(got, vec![json!({"id": 1, "ok": null})]);
    }
}
