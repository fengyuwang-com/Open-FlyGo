// Integration tests for the T-08 component-fetch page contract.
// Static-contract style: verifies the self-check page wires the fetch
// endpoints end to end without spawning processes or touching the network.

#[test]
fn fetch_card_is_present_in_system_view() {
    let html = include_str!("../static/index.html");
    assert!(html.contains("id=\"fetch-list\""));
    assert!(html.contains("id=\"fetch-all\""));
    assert!(html.contains("id=\"fetch-msg\""));
    assert!(html.contains("data-i18n=\"p.fetch\""));
}

#[test]
fn fetch_page_calls_both_endpoints() {
    let html = include_str!("../static/index.html");
    assert!(html.contains("'/api/fetch/components'"));
    assert!(html.contains("'/api/fetch/install'"));
    assert!(html.contains("async function loadFetch()"));
    assert!(html.contains("async function installComp(id)"));
}

#[test]
fn fetch_loader_is_polled_with_the_rest_of_the_page() {
    let html = include_str!("../static/index.html");
    // Initial load and the 15s poll must both refresh the component card.
    assert!(html.contains("loadSmb(),loadFetch()]"));
    assert!(html.contains("loadSmb();loadFetch()}"));
}

#[test]
fn fetch_install_buttons_are_delegated_and_guarded() {
    let html = include_str!("../static/index.html");
    assert!(html.contains("button[data-fid]"));
    assert!(html.contains("installComp(b.dataset.fid)"));
    // Manual-only and already-installed components must not render a button.
    assert!(html.contains("c.install_via!=='manual'"));
    assert!(html.contains("c.installed||c.install_via==='manual'"));
}

#[test]
fn fetch_i18n_keys_exist_in_both_languages() {
    let html = include_str!("../static/index.html");
    for key in [
        "fetch.loading",
        "fetch.all",
        "fetch.okSub",
        "fetch.missSub",
        "fetch.installed",
        "fetch.missing",
        "fetch.install",
        "fetch.installing",
        "fetch.allDone",
    ] {
        let needle_zh = format!("'{key}':");
        assert_eq!(
            html.matches(&needle_zh).count(),
            2,
            "i18n key {key} must exist in both zh and en"
        );
    }
}
