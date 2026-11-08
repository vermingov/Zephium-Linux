//! A page's own srcdoc, data: and blob: frames.
//!
//! WebKitGTK names no frame when it asks whether a navigation may go ahead,
//! so the navigation handler lets these documents through as about:blank, as
//! macOS does for a subframe; challenge and payment widgets are built from
//! them. Only the response says which frame loads, so a tab's own document
//! is refused here instead, as it would have been before.

use webkit2gtk::glib::object::Cast;
use webkit2gtk::{
    PolicyDecisionExt, PolicyDecisionType, ResponsePolicyDecision, ResponsePolicyDecisionExt, URIRequestExt, WebView,
    WebViewExt,
};

pub(crate) fn keep_out_of_main_frame(view: &WebView) {
    view.connect_decide_policy(|_, decision, kind| {
        if kind != PolicyDecisionType::Response {
            return false;
        }
        let Some(response) = decision.dynamic_cast_ref::<ResponsePolicyDecision>() else {
            return false;
        };
        if !response.is_main_frame_main_resource() {
            return false;
        }
        let url = response.request().and_then(|request| request.uri()).map(|uri| uri.to_string()).unwrap_or_default();
        if !refused_in_main_frame(&url) {
            return false;
        }
        decision.ignore();
        true
    });
}

/// The documents the navigation handler admits only for a subframe.
fn refused_in_main_frame(url: &str) -> bool {
    zephium_core::navigation::is_subframe_document(url) && url != "about:blank"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_frame_documents_are_kept_out() {
        assert!(refused_in_main_frame("about:srcdoc"));
        assert!(refused_in_main_frame("data:text/html,hi"));
        assert!(refused_in_main_frame("blob:https://example.com/5f0c"));
        assert!(!refused_in_main_frame("about:blank"));
        assert!(!refused_in_main_frame("https://example.com/"));
    }
}
