// Focused regression checks for the access-control page contract.
// These checks do not start the panel or touch Windows firewall state.

#[test]
fn access_page_uses_save_action_and_multi_select_controls() {
    let html = include_str!("../static/index.html");
    assert!(html.contains("id=\"ac-save\""));
    assert!(!html.contains("id=\"ac-add\""));
    assert!(html.contains("data-i18n=\"ac.save\""));
    assert!(html.matches("name=\"ac-scope\"").count() >= 4);
    assert!(html.contains("type=\"checkbox\" name=\"ac-scope\""));
    assert!(html.contains("onclick=()=>saveAccess()"));
    assert!(html.contains("scopes,cidr,explicit"));
    assert!(html.contains("/api/access/allow"));
    assert!(html.contains("await loadAccess()"));
}

#[test]
fn access_rules_stay_read_from_firewall_with_short_lived_cache() {
    // The access-control page must reflect real firewall state. A short-lived
    // process cache (RULES_CACHE_TTL) is tolerated because it expires quickly
    // and is invalidated by every successful write (run_op), so the firewall
    // remains the durable source of truth.
    let source = include_str!("../src/api/access.rs");
    assert!(source.contains("durable source of truth"));
    assert!(source.contains("Get-NetFirewallRule -DisplayName 'FlyGo-*'"));
    assert!(source.contains("RULES_CACHE_TTL"));
    assert!(source.contains("invalidate_rules_cache"));
}

#[test]
fn current_view_is_restored_and_persisted() {
    let html = include_str!("../static/index.html");
    assert!(html.contains("location.hash.startsWith(VIEW_HASH_PREFIX)"));
    assert!(html.contains("localStorage.getItem('flygo.currentView')"));
    assert!(html.contains("localStorage.setItem('flygo.currentView',v)"));
    assert!(html.contains("switchView(savedView(),false)"));
}

#[test]
fn access_page_keeps_conditional_scope_controls() {
    let html = include_str!("../static/index.html");
    assert!(html.contains("ac-cidr-row"));
    assert!(html.contains("ac-all-warn"));
    assert!(html.contains("selected.includes('lan')"));
    assert!(html.contains("selected.includes('all')"));
}
