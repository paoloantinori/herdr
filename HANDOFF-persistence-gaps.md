# Handoff: agent-name restore gaps in 3775aca4

Three correctness gaps from the high code-review of commit 3775aca4
(restore unmanaged agent names across daemon restarts). The commit covers
imported panes and panes with a pending resume plan; these cases remain.

## Findings (fix before push/merge)

1. restore.rs ~712, second match: a NON-imported pane with a saved
   agent_name but no resume plan still hits `(Some(_), None) => {}` and
   loses its name (resume disabled, or session duplicate-suppressed).
2. restore.rs ~609: `set_agent_name` with `restored_agent_session: None`
   leaves agent_name_owner None (hook_authority is None at restore, no
   agent detected yet): the first reporter on the pane adopts the
   restored name instead of the persisted session.
3. restore.rs ~606: restore calls set_agent_name/restore_managed_agent
   without the `agent_name_conflicts` duplicate-name check that guards
   live rename (agents.rs:108) and start (agents.rs:168): two panes with
   the same agent_name in one snapshot restore both, and
   `agent.get`/`prompt` by that name become Ambiguous.

## Suggested shape (from the review)

One helper, applied uniformly to all three restore paths:

    fn restore_saved_identity(
        terminal, saved_agent_name, saved_managed_agent,
        supersede: bool, persisted_session: SessionId,
    )

binding the name owner EXPLICITLY to the persisted session (finding 2),
plus a duplicate-name check against the restored snapshot before binding
(finding 3), and the missing non-imported arm (finding 1).

## Tests to add (session_restore.rs)

- non-imported pane with saved name, no resume plan: name restored.
- name persisted, no agent_session snapshot: owner binds to persisted
  session; a different session reporting later clears/rebinds.
- two panes with the same agent_name in one snapshot: both restore, the
  duplicate is flagged (or second auto-suffixed), agent.get is not
  Ambiguous.

## Gates

cargo test (session_restore + unit); commit conventional lowercase on
fork/v0.9.5-rebase; push needs the maintainer's ok.
