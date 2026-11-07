//! End-to-end check of the extension channel in a real, sandboxed WebKitGTK.
//!
//!   channel_probe <directory holding only libzephium_webext_process.so>
//!
//! Needs a display (the port's tools/rig, or xvfb-run). Prints one PASS or
//! FAIL line per claim in docs/extensions.md and exits non-zero on any FAIL.

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn main() {}

#[cfg(all(unix, not(target_os = "macos")))]
fn main() {
    probe::run();
}

#[cfg(all(unix, not(target_os = "macos")))]
mod probe {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use gtk::gio;
    use gtk::glib::{self, ToVariant};
    use gtk::prelude::*;
    use webkit2gtk::{
        SecurityManagerExt, URISchemeRequestExt, UserMessage, UserMessageExt, WebContext, WebContextExt,
        WebView, WebViewExt,
    };
    use zephium_webext_process::{InitExtension, EVENT_MESSAGE, KIND_CONTENT, KIND_PAGE, SEND_MESSAGE};

    const ENABLED: &str = "abcdefghijklmnopabcdefghijklmnop";
    const DISABLED: &str = "ponmlkjihgfedcbaponmlkjihgfedcba";
    const LIMIT: u32 = 64 * 1024;

    const EXTENSION_PAGE: &str = r#"<!doctype html><script>
        __zephiumWebextSend("page-info:" + __zephiumWebextContext, () => {});
        __zephiumWebextSend("from-page", answer => __zephiumWebextSend("page-got:" + answer, () => {}));
    </script>"#;

    const CONTENT_START: &str = r#"(() => {
        let big = "sent", wrong = "sent", kept = "kept";
        try { __zephiumWebextSend("x".repeat(70000), () => {}); } catch (e) { big = "threw"; }
        try { __zephiumWebextSend({}, () => {}); } catch (e) { wrong = "threw"; }
        try { __zephiumWebextSend = null; } catch (e) {}
        if (typeof __zephiumWebextSend !== "function") kept = "replaced";
        const report = ["from-content", big, wrong, kept].join("|");
        __zephiumWebextSend(report, answer => __zephiumWebextSend("content-got:" + answer, () => {}));
        __zephiumWebextSend("info:" + JSON.parse(__zephiumWebextInfo).id + ":" + __zephiumWebextContext, () => {});
        __zephiumWebextListen(text => __zephiumWebextSend("event:" + text, () => {}));
    })();"#;

    const CONTENT_END: &str = r#"__zephiumWebextSend("end:" + (document.body ? "body" : "nobody"), () => {});"#;
    const CONTENT_EXCLUDED: &str = r#"__zephiumWebextSend("excluded-ran", () => {});"#;

    const WEB_PAGE: &str = r#"<!doctype html><title>t</title><body><script>
        document.title = "pageworld:" + typeof window.__zephiumWebextSend;
    </script></body>"#;

    type Received = Rc<RefCell<Vec<(String, String, String)>>>;

    fn extensions() -> Vec<InitExtension> {
        let page = |pattern: &str| vec![pattern.to_owned()];
        let group = |matches: Vec<String>, excludes: Vec<String>, script: &str, run_at: &str| {
            (matches, excludes, vec![script.to_owned()], run_at.to_owned(), false)
        };
        let content = "file:///*/content.html";
        vec![(
            ENABLED.to_owned(),
            format!(r#"{{"id":"{ENABLED}"}}"#),
            vec!["<all_urls>".to_owned()],
            vec![
                group(page(content), vec![], CONTENT_START, "document_start"),
                group(page(content), vec![], CONTENT_END, "document_end"),
                group(page(content), page(content), CONTENT_EXCLUDED, "document_start"),
            ],
        )]
    }

    pub fn run() {
        let plugin_dir = std::env::args().nth(1).expect("usage: channel_probe <plugin directory>");
        gtk::init().expect("a display (run inside tools/rig or xvfb-run)");
        let pages = write_pages();

        let context = WebContext::new();
        context.set_sandbox_enabled(true);
        context.set_web_extensions_directory(&plugin_dir);
        let init = (LIMIT, extensions()).to_variant();
        assert_eq!(init.type_().as_str(), zephium_webext_process::INIT_DATA_TYPE);
        context.connect_initialize_web_extensions(move |context| {
            context.set_web_extensions_initialization_user_data(&init);
        });
        context.register_uri_scheme("chrome-extension", |request| {
            let bytes = glib::Bytes::from_static(EXTENSION_PAGE.as_bytes());
            let stream = gio::MemoryInputStream::from_bytes(&bytes);
            request.finish(&stream, bytes.len() as i64, Some("text/html"));
        });
        if let Some(security) = context.security_manager() {
            security.register_uri_scheme_as_secure("chrome-extension");
        }

        let view = WebView::with_context(&context);
        let received: Received = Rc::default();
        let log = received.clone();
        view.connect_user_message_received(move |_, message| {
            if message.name().as_deref() != Some(SEND_MESSAGE) {
                return false;
            }
            let Some((id, kind, _frame, body)) =
                message.parameters().and_then(|p| p.get::<(String, String, u64, String)>())
            else {
                return false;
            };
            let answer = UserMessage::new("reply", Some(&(format!("ok:{body}"),).to_variant()));
            message.send_reply(&answer);
            log.borrow_mut().push((id, kind, body));
            true
        });
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_default_size(800, 600);
        window.add(&view);
        window.show_all();

        let mut failures = 0;
        let mut check = |claim: &str, ok: bool| {
            println!("{} {claim}", if ok { "PASS" } else { "FAIL" });
            failures += usize::from(!ok);
        };

        view.load_uri(&format!("chrome-extension://{ENABLED}/page.html"));
        check("an enabled extension's page sends and gets its reply", wait(|| has(&received, KIND_PAGE, "page-got:ok:from-page")));
        check("an extension page is told it is a page", has(&received, KIND_PAGE, "page-info:page"));

        view.load_uri(&file_uri(&pages.join("content.html")));
        let report = "from-content|threw|threw|kept";
        check(
            "a matching page runs the content script, which sends and gets its reply",
            wait(|| has(&received, KIND_CONTENT, &format!("content-got:ok:{report}"))),
        );
        check("an oversized message is refused in the web process", has(&received, KIND_CONTENT, report));
        check("a non-string message is refused", has(&received, KIND_CONTENT, report));
        check("content-script code cannot replace the send function", has(&received, KIND_CONTENT, report));
        check(
            "a content script reads its extension's info and is told it is content",
            has(&received, KIND_CONTENT, &format!("info:{ENABLED}:content")),
        );
        check("document_end scripts run once the body is parsed", wait(|| has(&received, KIND_CONTENT, "end:body")));
        check("exclude_matches keeps a group out", !has(&received, KIND_CONTENT, "excluded-ran"));
        check(
            "the page's own scripts cannot see the send function",
            wait(|| view.title().as_deref() == Some("pageworld:undefined")),
        );

        let event = UserMessage::new(EVENT_MESSAGE, Some(&(ENABLED, 0u64, "ping").to_variant()));
        view.send_message_to_page(&event, None::<&gio::Cancellable>, |_| {});
        check("an event from the browser reaches the content script's listener", wait(|| has(&received, KIND_CONTENT, "event:ping")));
        check("every message carried the enabled extension's ID", received.borrow().iter().all(|(id, ..)| id == ENABLED));

        let before = received.borrow().len();
        view.load_uri(&file_uri(&pages.join("other.html")));
        let quiet = !wait_for(Duration::from_millis(1500), || received.borrow().len() > before);
        check("a page the patterns do not match gets no content script", quiet);

        view.load_uri(&format!("chrome-extension://{DISABLED}/page.html"));
        let quiet = !wait_for(Duration::from_millis(1500), || received.borrow().len() > before);
        check("a disabled extension's page gets no channel", quiet);

        std::process::exit(i32::from(failures > 0));
    }

    fn has(received: &Received, kind: &str, body: &str) -> bool {
        received.borrow().iter().any(|(_, k, b)| k == kind && b == body)
    }

    fn wait(condition: impl Fn() -> bool) -> bool {
        wait_for(Duration::from_secs(10), condition)
    }

    /// Run the main loop until `condition` holds or `limit` passes.
    fn wait_for(limit: Duration, condition: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            while gtk::events_pending() {
                gtk::main_iteration();
            }
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        condition()
    }

    fn write_pages() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zephium-channel-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temporary directory");
        std::fs::write(dir.join("content.html"), WEB_PAGE).expect("test page");
        std::fs::write(dir.join("other.html"), WEB_PAGE).expect("test page");
        dir
    }

    fn file_uri(path: &Path) -> String {
        format!("file://{}", path.display())
    }
}
