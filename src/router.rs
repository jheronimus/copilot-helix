//! Routes each JSON-RPC message to one of three actions.
//!
//! The routing table is static and keyed on `(method, Direction)`. As later
//! steps add protocol translation and initialization logic, they register new
//! entries by adding arms to `route()`.

/// Which direction a message is travelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Helix → language server.
    HelixToUpstream,
    /// Language server → Helix.
    UpstreamToHelix,
}

/// What the proxy should do with a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteAction {
    /// Forward the message to the other side unchanged.
    PassThrough,
    /// Swallow the message entirely; do not forward it.
    Drop,
    /// The proxy intercepts and handles this message itself.
    /// The caller is responsible for implementing the handler.
    Intercept,
}

/// Decide what to do with `method` travelling in `dir`.
///
/// `method` is `None` for responses (which are always passed through, since
/// the proxy correlates them by request id rather than method name).
pub fn route(method: Option<&str>, dir: Direction) -> RouteAction {
    let Some(method) = method else {
        return RouteAction::PassThrough;
    };

    match (method, dir) {
        ("textDocument/completion", Direction::HelixToUpstream) => RouteAction::Intercept,
        ("$/cancelRequest", Direction::HelixToUpstream) => RouteAction::Intercept,

        // Drop these so they never reach Helix and cause spurious errors.
        ("$/logTrace", Direction::UpstreamToHelix) => RouteAction::Drop,
        ("$/progress", Direction::UpstreamToHelix) => RouteAction::Drop,
        ("featureFlagsNotification", Direction::UpstreamToHelix) => RouteAction::Drop,
        ("didChangeStatus", Direction::UpstreamToHelix) => RouteAction::Drop,
        ("statusNotification", Direction::UpstreamToHelix) => RouteAction::Drop,
        ("policy/didChange", Direction::UpstreamToHelix) => RouteAction::Drop,
        ("copilot/mcpTools", Direction::UpstreamToHelix) => RouteAction::Drop,
        ("conversation/preconditionsNotification", Direction::UpstreamToHelix) => {
            RouteAction::Drop
        }

        _ => RouteAction::PassThrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_always_pass_through() {
        assert_eq!(
            route(None, Direction::HelixToUpstream),
            RouteAction::PassThrough
        );
        assert_eq!(
            route(None, Direction::UpstreamToHelix),
            RouteAction::PassThrough
        );
    }

    #[test]
    fn unknown_methods_pass_through() {
        assert_eq!(
            route(Some("textDocument/hover"), Direction::HelixToUpstream),
            RouteAction::PassThrough
        );
        assert_eq!(
            route(Some("textDocument/definition"), Direction::UpstreamToHelix),
            RouteAction::PassThrough
        );
    }

    #[test]
    fn copilot_internal_notifications_dropped() {
        for method in &[
            "featureFlagsNotification",
            "didChangeStatus",
            "statusNotification",
            "policy/didChange",
            "copilot/mcpTools",
            "conversation/preconditionsNotification",
            "$/logTrace",
            "$/progress",
        ] {
            assert_eq!(
                route(Some(method), Direction::UpstreamToHelix),
                RouteAction::Drop,
                "expected {method} to be dropped"
            );
        }
    }

    #[test]
    fn copilot_internals_not_dropped_in_wrong_direction() {
        // These come from upstream; if Helix somehow sent them they'd pass through.
        assert_eq!(
            route(Some("featureFlagsNotification"), Direction::HelixToUpstream),
            RouteAction::PassThrough
        );
    }
}
