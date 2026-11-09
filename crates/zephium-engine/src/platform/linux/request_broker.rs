//! The browser's end of blocking `webRequest`: the socket the web-process
//! plugin asks on whether a page's request may go out. Why it is a socket,
//! and the line protocol, are in zephium-webext-process's requests.rs.
//!
//! The socket lives in a directory of its own under the runtime directory,
//! readable only by this user; the content-process sandbox gets that
//! directory read-only, which is enough to connect. Each connection (one per
//! web process) is read on a thread of its own, since the plugin blocks on
//! it; the question goes to the engine host on the main thread, and the
//! thread writes whatever answer comes back, or `allow` when none does in
//! time.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::Duration;

/// How long a question waits for the extensions. The plugin waits a little
/// longer, so it hears this answer rather than its own timeout.
const WAIT: Duration = Duration::from_millis(2_500);
/// The longest question read: six fields, two of them URLs.
const QUESTION_LIMIT: u64 = 256 * 1024;

/// A request a page is about to make.
#[derive(Debug)]
pub(crate) struct Question {
    pub(crate) page_id: u64,
    /// 0 for the page's own document and what it loads; a number of the
    /// plugin's for a frame's document.
    pub(crate) frame_id: u64,
    /// `webRequest.ResourceType`, Firefox's names.
    pub(crate) kind: String,
    pub(crate) method: String,
    pub(crate) url: String,
    /// The address of the page making the request.
    pub(crate) document: String,
}

pub(crate) enum Decision {
    Allow,
    Cancel,
    Redirect(String),
}

/// A question waiting for the extensions. Dropping it unanswered lets the
/// request go.
pub(crate) struct Ask {
    pub(crate) question: Question,
    reply: mpsc::Sender<Decision>,
}

impl Ask {
    pub(crate) fn answer(self, decision: Decision) {
        let _ = self.reply.send(decision);
    }
}

static SOCKET: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The socket's path, serving from the first call on; `None` when it cannot
/// be made, which leaves blocking `webRequest` unavailable. The sandbox must
/// show the content processes its directory.
pub(crate) fn serve() -> Option<&'static PathBuf> {
    SOCKET.get_or_init(|| start().inspect_err(|error| eprintln!("extensions: no webRequest socket: {error}")).ok()).as_ref()
}

fn start() -> std::io::Result<PathBuf> {
    let directory = gtk::glib::user_runtime_dir().join(format!("app.zephium-webrequest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let path = directory.join("socket");
    let listener = UnixListener::bind(&path)?;
    std::thread::Builder::new().name("webrequest-accept".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = std::thread::Builder::new().name("webrequest".into()).spawn(move || answer_questions(stream));
        }
    })?;
    Ok(path)
}

fn answer_questions(stream: UnixStream) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.by_ref().take(QUESTION_LIMIT).read_line(&mut line) {
            Ok(length) if length > 0 && line.ends_with('\n') => {}
            _ => return,
        }
        let decision = match parse(line.trim_end_matches('\n')) {
            Some(question) => decide(question),
            None => Decision::Allow,
        };
        let answer = match decision {
            Decision::Allow => "allow\n".to_owned(),
            Decision::Cancel => "cancel\n".to_owned(),
            Decision::Redirect(target) if !target.contains(['\t', '\n', '\r']) => format!("redirect\t{target}\n"),
            Decision::Redirect(_) => "allow\n".to_owned(),
        };
        if writer.write_all(answer.as_bytes()).is_err() {
            return;
        }
    }
}

fn decide(question: Question) -> Decision {
    let (reply, decision) = mpsc::channel();
    let ask = Ask { question, reply };
    gtk::glib::MainContext::default().invoke(move || {
        let _ = crate::host::try_with(move |host| host.linux_web_request(ask));
    });
    decision.recv_timeout(WAIT).unwrap_or(Decision::Allow)
}

fn parse(line: &str) -> Option<Question> {
    let mut fields = line.splitn(6, '\t');
    let page_id = fields.next()?.parse().ok()?;
    let frame_id = fields.next()?.parse().ok()?;
    let kind = fields.next()?.to_owned();
    let method = fields.next()?.to_owned();
    let url = fields.next()?.to_owned();
    let document = fields.next()?.to_owned();
    Some(Question { page_id, frame_id, kind, method, url, document })
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn a_question_has_six_fields() {
        let question = parse("7\t0\tscript\tGET\thttps://ads.test/a.js\thttps://site.test/").unwrap();
        assert_eq!((question.page_id, question.frame_id), (7, 0));
        assert_eq!(question.kind, "script");
        assert_eq!(question.url, "https://ads.test/a.js");
        assert_eq!(question.document, "https://site.test/");
        assert!(parse("7\t0\tscript\tGET\thttps://ads.test/a.js").is_none());
        assert!(parse("x\t0\tscript\tGET\tu\td").is_none());
    }
}
