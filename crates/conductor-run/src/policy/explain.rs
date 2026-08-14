//! Rendering a decision for a human — `conductor policy explain <action>`.
//!
//! §4.4 line 633: *"prints: action · resolved effect · the ceiling that applied ·
//! every rule that matched **and every rule considered that did not, with the
//! reason** · facts and their sources · policy hash · any exception with scope
//! and expiry. **Negative results are what people debug.**"*
//!
//! # This module computes nothing
//!
//! It renders a [`Decision`] and never re-derives one. A second evaluation path
//! inside `explain` would eventually disagree with the first, and at that point
//! the explanation stops being a record of what happened and becomes a plausible
//! story about it — which is worse than no explanation, because it is trusted.
//!
//! # One line per rule
//!
//! Each considered rule prints its id and its reason on the same line. A reader
//! debugging at 2 a.m. greps for a rule id; a layout that puts the reason on the
//! next line makes that grep return the half without the answer.

use super::evaluate::Decision;
use super::model::Effect;

/// Render a decision as text.
pub fn render(decision: &Decision) -> String {
    let mut out = String::new();

    out.push_str(&format!("action:  {}\n", decision.action));
    out.push_str(&format!("effect:  {}\n", decision.effect));
    if decision.unknown_action {
        out.push_str(
            "         (this action is outside Conductor's taxonomy; §4.4 fails \
             closed, so it denies)\n",
        );
    }
    out.push_str(&format!("policy:  {}\n", decision.policy_hash));

    let ceiling = if decision.ceiling_rules.is_empty() {
        format!("{} (no locked rule applied)", decision.ceiling)
    } else {
        format!(
            "{} (from {})",
            decision.ceiling,
            decision.ceiling_rules.join(", ")
        )
    };
    out.push_str(&format!("ceiling: {ceiling}\n"));

    out.push_str("\nrules that matched:\n");
    if decision.matched.is_empty() {
        out.push_str("  (none)\n");
    }
    for rule in &decision.matched {
        let lock = if rule.locked { " locked" } else { "" };
        let capped = if rule.contributed == rule.declared {
            String::new()
        } else {
            format!(" — capped to {}", rule.contributed)
        };
        out.push_str(&format!(
            "  [{}{}] {} — {} → {}{}\n",
            rule.origin, lock, rule.rule_id, rule.pattern, rule.declared, capped
        ));
    }

    out.push_str("\nrules considered that did not match:\n");
    if decision.not_matched.is_empty() {
        out.push_str("  (none)\n");
    }
    for rule in &decision.not_matched {
        out.push_str(&format!(
            "  [{}] {} — {} → {} — {}\n",
            rule.origin, rule.rule_id, rule.pattern, rule.declared, rule.reason
        ));
    }

    out.push_str("\nbuilt-in invariants (not configurable, §4.4):\n");
    if decision.invariants.is_empty() {
        out.push_str("  (none applied)\n");
    }
    for invariant in &decision.invariants {
        let resting = if invariant.supporting.is_empty() {
            "on the action itself".to_string()
        } else {
            format!("on {}", invariant.supporting.join(", "))
        };
        out.push_str(&format!(
            "  {} → {} (resting {resting})\n",
            invariant.id, invariant.contributed
        ));
    }

    out.push_str("\nexception:\n");
    match &decision.exception {
        None => out.push_str("  (none applied)\n"),
        Some(exception) => {
            let clamped = if exception.granted == exception.requested {
                String::new()
            } else {
                format!(
                    " — clamped to {} by the ceiling, the built-in invariants or \
                     the unknown-action floor",
                    exception.granted
                )
            };
            out.push_str(&format!(
                "  [{}] {} — {} requested {} scope {} expires_at_ms {}{}\n",
                exception.origin,
                exception.id,
                exception.action,
                exception.requested,
                exception.scope,
                exception.expires_at_ms,
                clamped
            ));
        }
    }

    out.push_str("\nfacts:\n");
    if decision.facts.is_empty() {
        out.push_str("  (none)\n");
    }
    for fact in &decision.facts {
        out.push_str(&format!(
            "  {} = {} [{}]\n",
            fact.key, fact.value, fact.source
        ));
    }

    if !decision.caps.is_empty() {
        out.push_str("\ndenies capped — §4.4: a deny must rest only on deterministic facts:\n");
        for cap in &decision.caps {
            out.push_str(&format!(
                "  {} capped from deny to {} because {} is {}\n",
                cap.source_id, cap.capped_to, cap.fact_key, cap.fact_source
            ));
        }
    }

    out.push_str(&format!("\nresolved: {}\n", decision.effect));
    if decision.effect == Effect::RequireApproval {
        out.push_str("S8 owns the asking; this slice only decides that it must be asked.\n");
    }
    out
}
