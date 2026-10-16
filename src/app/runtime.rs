use std::time::{Duration, Instant, SystemTime};

use super::{
    background_update_check_enabled, App, AUTO_UPDATE_CHECK_INTERVAL, MIN_RENDER_INTERVAL,
};
fn retain_detached_process_after_wait(
    pid: u32,
    result: std::io::Result<Option<std::process::ExitStatus>>,
) -> bool {
    match result {
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
        Err(err) => {
            tracing::warn!(pid, err = %err, "failed to reap detached process");
            false
        }
    }
}

pub(crate) const ARCHIVE_SWEEP_STARTUP_DELAY: Duration = Duration::from_secs(5 * 60);
pub(crate) const ARCHIVE_SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
pub(crate) const AUTO_ARCHIVE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(crate) const RESTORE_SETTLE: Duration = Duration::from_secs(60);

impl App {
    pub(crate) fn reap_finished_detached_processes(&mut self) {
        self.detached_process_children
            .retain_mut(|child| retain_detached_process_after_wait(child.id(), child.try_wait()));
    }

    pub(crate) fn shutdown_terminal_runtime(&mut self, terminal_id: crate::terminal::TerminalId) {
        if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
            runtime.shutdown();
        }
    }

    pub(crate) fn shutdown_detached_terminal_runtimes(&mut self) {
        let terminal_ids = std::mem::take(&mut self.state.terminal_runtime_shutdowns);
        for terminal_id in terminal_ids {
            self.shutdown_terminal_runtime(terminal_id);
        }
    }

    pub(crate) fn sync_agent_metadata_deadline(&mut self) {
        self.agent_metadata_deadline = self.state.next_agent_metadata_expiry();
    }

    pub(crate) fn expire_due_metadata(&mut self, now: Instant) -> bool {
        let Some(deadline) = self
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        else {
            return false;
        };
        self.expire_metadata_at(deadline, now);
        true
    }

    pub(crate) fn expire_metadata_at(&mut self, deadline: Instant, now: Instant) {
        let previous_toast = self.state.toast.clone();
        for update in self.state.expire_agent_metadata_at(deadline, now) {
            self.refresh_new_herdr_toast_context_for_update(&update, &previous_toast);
            self.emit_pane_state_update(&update);
        }
        let (panes, workspaces) = self.state.expire_metadata_tokens(now);
        for (ws_idx, pane_id) in panes {
            self.emit_pane_updated(ws_idx, pane_id);
        }
        for ws_idx in workspaces {
            self.emit_workspace_token_updated(ws_idx);
        }
        self.sync_agent_metadata_deadline();
    }

    pub(crate) fn run_archive_sweep(&mut self, now: Instant) -> bool {
        if now < self.next_archive_sweep {
            return false;
        }
        self.next_archive_sweep = now + ARCHIVE_SWEEP_INTERVAL;
        let archived = self.state.archive_inactive_workspaces(SystemTime::now());
        self.emit_archive_events();
        // Persist refreshed last_activity_at values at least once per sweep.
        self.state.mark_session_dirty();
        archived
    }

    pub(crate) fn emit_archive_events(&mut self) {
        for index in std::mem::take(&mut self.state.archive_transitions) {
            let Some(workspace) = self.state.workspaces.get(index) else {
                continue;
            };
            let archived = workspace.archived_at.is_some();
            self.emit_event(crate::api::schema::EventEnvelope {
                event: crate::api::schema::EventKind::WorkspaceArchiveChanged,
                data: crate::api::schema::EventData::WorkspaceArchiveChanged {
                    workspace_id: self.public_workspace_id(index),
                    archived,
                },
            });
        }
    }

    /// Input-path wrapper around [`AppState::note_workspace_input`] that also emits events.
    pub(crate) fn note_workspace_input(&mut self, ws_idx: usize) -> bool {
        let unarchived = self.state.note_workspace_input(ws_idx);
        self.emit_archive_events();
        unarchived
    }

    pub(crate) fn can_render_now(&self, now: Instant) -> bool {
        match self.last_render_at {
            Some(last_render_at) => now.duration_since(last_render_at) >= MIN_RENDER_INTERVAL,
            None => true,
        }
    }

    pub(crate) fn can_present_now(&self, now: Instant) -> bool {
        match self.last_presentation_at {
            Some(last_presentation_at) => {
                now.duration_since(last_presentation_at) >= MIN_RENDER_INTERVAL
            }
            None => true,
        }
    }

    pub(crate) fn record_render_attempt(&mut self, now: Instant, presentation: bool) {
        self.last_render_at = Some(now);
        if presentation {
            self.last_presentation_at = Some(now);
        }
    }

    pub(crate) fn run_auto_update_check(&mut self) {
        if !background_update_check_enabled(
            self.policy.background_updates,
            self.update_version_check_enabled,
        ) {
            self.next_auto_update_check = None;
            return;
        }

        self.next_auto_update_check = self
            .state
            .update_available
            .is_none()
            .then_some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        if self.state.update_available.is_some() {
            return;
        }

        let update_tx = self.event_tx.clone();
        if let Err(err) = crate::thread_spawn::spawn_named("herdr-update-check", move || {
            crate::update::auto_update(update_tx)
        }) {
            tracing::warn!(err = %err, "failed to spawn update check thread");
        }
    }

    pub(crate) fn run_agent_manifest_update_check(&mut self) {
        if !background_update_check_enabled(
            self.policy.background_updates,
            self.update_manifest_check_enabled,
        ) {
            self.next_agent_manifest_update_check = None;
            return;
        }

        self.next_agent_manifest_update_check = Some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        let manifest_update_tx = self.event_tx.clone();
        if let Err(err) = crate::thread_spawn::spawn_named("herdr-manifest-check", move || {
            crate::detect::manifest_update::auto_update(manifest_update_tx)
        }) {
            tracing::warn!(err = %err, "failed to spawn agent manifest check thread");
        }
    }

    pub(crate) fn next_headless_loop_deadline_with_git_refresh(
        &self,
        now: Instant,
        needs_render: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        let render_deadline = if needs_render {
            self.last_render_at
                .map(|last_render_at| last_render_at + MIN_RENDER_INTERVAL)
                .filter(|deadline| *deadline > now)
        } else {
            None
        };

        [
            self.config_diagnostic_deadline,
            self.toast_deadline,
            self.state.next_pending_agent_notification_deadline(),
            self.state.next_managed_agent_deadline(),
            include_git_refresh
                .then(|| self.git_refresh_deadline())
                .flatten(),
            self.restored_worktree_validation_retry_at,
            self.next_auto_update_check,
            self.next_agent_manifest_update_check,
            self.agent_metadata_deadline,
            self.pending_agent_resume_deadline,
            self.pending_host_appearance_deadline,
            self.session_save_deadline,
            self.next_tab_bar_status_deadline(),
            render_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    #[cfg(test)]
    pub(crate) fn drain_internal_events(&mut self) -> bool {
        self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT)
            .1
    }

    #[cfg(test)]
    pub(crate) fn drain_all_internal_events(&mut self) -> bool {
        let mut changed = false;
        loop {
            let (had_event, batch_changed) =
                self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT);
            changed |= batch_changed;
            if !had_event {
                break;
            }
        }
        changed
    }

    #[cfg(test)]
    fn drain_internal_events_up_to(&mut self, limit: usize) -> (bool, bool) {
        let mut had_event = false;
        let mut changed = false;
        for _ in 0..limit {
            let Ok(ev) = self.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            changed |= self.handle_internal_event_with_render_impact(ev);
        }
        (had_event, changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    #[test]
    fn hidden_render_attempt_keeps_presentation_cadence_available() {
        let (mut app, _) = test_app_with_pane();
        let initial_presentation = Instant::now();
        app.record_render_attempt(initial_presentation, true);

        let hidden_attempt = initial_presentation + MIN_RENDER_INTERVAL;
        app.record_render_attempt(hidden_attempt, false);
        let foreground_echo = hidden_attempt + Duration::from_millis(1);

        assert!(!app.can_render_now(foreground_echo));
        assert!(app.can_present_now(foreground_echo));
    }

    #[test]
    fn interrupted_detached_process_wait_keeps_child_for_retry() {
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "test interrupt");

        assert!(retain_detached_process_after_wait(42, Err(interrupted)));
    }

    fn test_app_with_pane() -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        });
        (app, pane_id)
    }
}
