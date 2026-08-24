# Deleted unit-test coverage ledger

Every in-process unit test was deleted from this repository on 2026-08-15 per
the AGENTS.md "Writing tests" policy. This file tracks what those tests
claimed to cover so the permitted test kinds (e2e server+mock-backend, real
backend conformance, wasm UI, dev-instance) can be checked for equivalent
coverage. Check an item off only when a permitted test demonstrably covers
the same behavior — or when the behavior is judged not worth covering.

Totals: 1,474 test functions in 118 files (~68k lines), plus 4 standalone
test files and ~230 test-only plumbing items (gates, hooks, cfg twins).

## Deleted test modules and their tests

### agent-adapter

**`agent-adapter/src/certification.rs`**

- mod `tests` (2 tests):
  - [ ] `certification_case_ids_are_unique`
  - [ ] `every_declared_capability_has_a_certification_case`

**`agent-adapter/src/conformance.rs`**

- mod `tests` (26 tests):
  - [ ] `accepts_a_well_formed_user_turn`
  - [ ] `rejects_an_unprompted_turn_without_agent_initiation`
  - [ ] `accepts_an_agent_initiated_turn_after_idle`
  - [ ] `rejects_idle_tool_progress_without_background_tasks`
  - [ ] `accepts_idle_tool_progress_for_background_tasks`
  - [ ] `rejects_mismatched_stream_ids`
  - [ ] `rejects_reusing_a_terminal_stream_id`
  - [ ] `rejects_idle_transition_with_an_open_tool`
  - [ ] `enforces_cancellation_ordering_and_uniqueness`
  - [ ] `advertised_turn_usage_requires_evidence`
  - [ ] `advertised_turn_usage_accepts_known_message_usage`
  - [ ] `model_request_usage_satisfies_turn_and_context_capabilities`
  - [ ] `rejects_model_request_sequence_gaps`
  - [ ] `rejects_decreasing_cumulative_usage`
  - [ ] `replay_does_not_require_accepted_input`
  - [ ] `reported_context_breakdown_is_required_when_advertised`
  - [ ] `finish_rejects_an_active_turn`
  - [ ] `finish_rejects_an_accepted_input_that_never_started`
  - [ ] `background_task_requires_running_then_one_terminal_status`
  - [ ] `task_updates_require_capability_and_monotonic_snapshots`
  - [ ] `task_replacement_and_clear_require_declared_capabilities`
  - [ ] `orchestration_requires_start_before_terminal_and_exactly_one_terminal`
  - [ ] `orchestration_fanout_correlates_workers_and_order`
  - [ ] `compaction_markers_require_capability_and_unique_correlated_terminals`
  - [ ] `replay_validates_independently_without_duplicate_live_false_positives`
  - [ ] `retry_telemetry_requires_capability_and_contiguous_stable_attempts`

**`agent-adapter/src/coverage.rs`**

- mod `tests` (22 tests):
  - [ ] `every_generic_request_has_an_explicit_live_capability`
  - [ ] `generic_tool_lifecycle_matrix_enumerates_each_meaningful_cell_once`
  - [ ] `retry_lifecycle_matrix_classifies_every_real_transport_boundary`
  - [ ] `request_usage_lifecycle_requires_each_reachable_shape`
  - [ ] `matrix_includes_user_response_while_background_work_runs`
  - [ ] `every_tool_variant_state_stimulus_combination_is_classified`
  - [ ] `every_declared_variant_has_required_coverage`
  - [ ] `required_cells_are_generated_uniquely`
  - [ ] `agent_control_coverage_declares_every_independent_dimension`
  - [ ] `agent_control_coverage_is_the_valid_cartesian_product`
  - [ ] `progress_coverage_crosses_modes_and_boundaries`
  - [ ] `task_update_lifecycle_classifies_every_row_truthfully`
  - [ ] `normalized_turn_coverage_declares_all_shapes_and_boundaries`
  - [ ] `enumerated_live_ledgers_reject_missing_cells`
  - [ ] `ambient_boundary_outcomes_are_explicit`
  - [ ] `await_contract_includes_independent_ambient_activity`
  - [ ] `await_completion_applies_when_any_watched_child_becomes_ready`
  - [ ] `rendezvous_terminal_timing_applies_to_every_lifecycle_stimulus`
  - [ ] `matrix_does_not_generate_impossible_blind_cross_products`
  - [ ] `incomplete_ledger_fails_closed`
  - [ ] `complete_ledger_passes`
  - [ ] `input_admission_matrix_is_capability_gated_and_fails_closed`

**`agent-adapter/src/lib.rs`**

- mod `tests` (4 tests):
  - [ ] `capabilities_are_unique_and_queryable`
  - [ ] `reported_context_breakdown_requires_reported_context_usage`
  - [ ] `startup_mcp_requires_generic_other_normalization`
  - [ ] `model_request_usage_requires_turn_usage`

### client

**`client/src/lib.rs`**

- mod `voice_tests` (3 tests):
  - [ ] `server_voice_frames_require_typed_host_and_session_routes`
  - [ ] `server_voice_rejects_wrong_routes_binary_and_input_audio`
  - [ ] `envelope_only_api_validates_and_skips_typed_voice_events`

**`client/src/runtime.rs`**

- mod `compaction_session_tests` (2 tests):
  - [ ] `compaction_events_resolve_pending_agent_session_identity`
  - [ ] `bootstrap_folds_compaction_sessions_and_rejects_conflicts`

### dev-driver

**`dev-driver/src/agent_control.rs`**

- mod `tests` (20 tests):
  - [ ] `spawn_tool_input_rejects_unknown_fields`
  - [ ] `spawn_tool_schema_exposes_launch_profiles_session_settings_and_hermes`
  - [ ] `spawn_tool_input_accepts_schema_advertised_access_mode`
  - [ ] `await_tool_schema_rejects_timeout_fields`
  - [ ] `await_tool_input_rejects_timeout_fields`
  - [ ] `read_tool_schemas_separate_latest_and_debug_controls`
  - [ ] `latest_read_input_rejects_debug_controls`
  - [ ] `compaction_notify_can_establish_missing_agent_session`
  - [ ] `team_compaction_member_must_match_agent_session`
  - [ ] `team_compaction_skips_unknown_host_scoped_members`
  - [ ] `agent_start_rejects_logical_session_rotation`
  - [ ] `agent_start_retains_deliberate_tolerance`
  - [ ] `latest_output_cache_does_not_look_back_past_empty_or_error`
  - [ ] `bootstrap_latest_error_wins_over_older_replayed_message`
  - [ ] `debug_read_byte_cap_advances_incremental_cursor`
  - [ ] `list_agents_excludes_agents_not_created_by_driver`
  - [ ] `await_agents_then_read_returns_completed_turn`
  - [ ] `await_agents_treats_late_replayed_completed_message_as_ready`
  - [ ] `await_tool_does_not_return_while_still_thinking`
  - [ ] `send_message_updates_existing_agent`

**`dev-driver/src/debug.rs`**

- mod `tests` (13 tests):
  - [ ] `evaluate_tool_input_rejects_unknown_fields`
  - [ ] `write_dev_config_overrides_frontend_port`
  - [ ] `tauri_dev_command_uses_resolved_cargo_tauri_without_install_fallback`
  - [ ] `cargo_tauri_resolution_uses_explicit_search_order`
  - [ ] `startup_diagnostics_include_bounded_output_tail`
  - [ ] `debug_capabilities_fail_closed_for_unsupported_surfaces`
  - [ ] `debug_events_exposes_resume_cursor_without_empty_event`
  - [ ] `dev_instance_environment_isolates_every_mutable_path`
  - [ ] `disposable_hermes_environment_is_attested_and_denies_provider_egress`
  - [ ] `start_tool_schema_exposes_opt_in_hermes_environment`
  - [ ] `toolchain_homes_survive_disposable_home_redirection`
  - [ ] `trunk_cache_survives_while_home_remains_isolated`
  - [ ] `dev_instance_seeds_only_requested_project`

### devtools-protocol

**`devtools-protocol/src/lib.rs`**

- mod `tests` (12 tests):
  - [ ] `request_round_trips`
  - [ ] `response_round_trips`
  - [ ] `dev_instance_mutable_paths_are_unique_and_confined`
  - [ ] `disposable_hermes_environment_is_confined_and_seeds_loopback_stub`
  - [ ] `disposable_hermes_environment_rejects_egress_and_profile_traversal`
  - [ ] `disposable_hermes_environment_requires_loopback_stub`
  - [ ] `parent_runtime_resolves_home_dependent_hermes_launcher`
  - [ ] `parent_runtime_resolves_home_dependent_python_and_executable`
  - [ ] `parent_runtime_preserves_python_venv_symlink_invocation`
  - [ ] `provider_environment_classifier_covers_credentials_and_endpoint_overrides`
  - [ ] `startup_cleanup_removes_every_tracked_artifact_unless_disarmed`
  - [ ] `bounded_debug_output_uses_monotonic_cursors_and_reports_loss`

### frontend

**`frontend/src/actions.rs`**

- mod `native_tests` (1 tests):
  - [ ] `side_open_with_navigation_targets_the_duplicate_occurrence`

**`frontend/src/app.rs`**

- mod `tests` (7 tests):
  - [ ] `recognizes_browser_file_drag_type`
  - [ ] `recognizes_external_chat_hrefs_case_insensitively`
  - [ ] `resolves_absolute_file_links_with_line_numbers`
  - [ ] `resolves_relative_file_links_with_line_and_column_numbers`
  - [ ] `resolves_percent_encoded_file_urls`
  - [ ] `rejects_absolute_paths_outside_the_active_project`
  - [ ] `host_failure_copy_uses_configured_label_or_neutral_fallback`
- mod `native_voice_probe_tests` (1 tests):
  - [ ] `native_voice_probe_disables_only_on_explicit_false`

**`frontend/src/code_intel_dom.rs`**

- mod `tests` (3 tests):
  - [ ] `utf16_col_maps_to_byte_offset_across_multibyte_chars`
  - [ ] `utf16_col_past_line_end_clamps_to_byte_len`
  - [ ] `identifier_byte_accepts_word_chars_and_rejects_punctuation`

**`frontend/src/components/agent_monitor_view.rs`**

- mod `tests` (8 tests):
  - [ ] `manual_order_uses_session_id_when_present`
  - [ ] `view_filters_apply_each_dimension`
  - [ ] `sort_modes_order_rows`
  - [ ] `manual_then_activity_freezes_ordered_rows_and_appends_new`
  - [ ] `grouping_buckets_in_first_seen_order`
  - [ ] `flat_grouping_is_single_unlabeled_group`
  - [ ] `reorder_moves_before_and_after_targets`
  - [ ] `reorder_ignores_missing_or_same_key`

**`frontend/src/components/agents_panel.rs`**

- mod `tests` (13 tests):
  - [ ] `cancelled_turn_is_not_reported_as_completed_idle`
  - [ ] `cancelled_outcome_yields_to_a_running_turn_and_is_absent_by_default`
  - [ ] `pending_interrupt_reports_cancelling_until_the_backend_answers`
  - [ ] `hide_sub_agents_drops_children_keeps_parents`
  - [ ] `hide_inactive_keeps_starting_streaming_and_turn_active`
  - [ ] `show_other_projects_off_on_home_keeps_only_none_project`
  - [ ] `show_other_projects_off_in_project_requires_host_and_project_match`
  - [ ] `show_other_projects_on_bypasses_project_check`
  - [ ] `search_matches_case_insensitively`
  - [ ] `defaults_for_home_shows_other_projects_true`
  - [ ] `defaults_for_specific_project_shows_other_projects_false`
  - [ ] `context_compaction_derives_queued_and_compacting_separately`
  - [ ] `compaction_status_labels_are_visible_without_hover`

**`frontend/src/components/command_palette.rs`**

- mod `tests` (5 tests):
  - [ ] `no_global_binding_claims_a_composer_chord`
  - [ ] `pane_management_installs_no_global_chords`
  - [ ] `global_bindings_do_not_collide`
  - [ ] `letter_keys_are_shown_uppercase_on_every_platform`
  - [ ] `shortcut_hints_come_from_the_bound_chord`

**`frontend/src/components/diff_view.rs`**

- mod `tests` (17 tests):
  - [ ] `worktree_scopes_are_eligible_without_a_status_entry`
  - [ ] `staged_is_eligible_when_the_file_has_nothing_unstaged`
  - [ ] `staged_is_ineligible_once_the_file_has_unstaged_changes`
  - [ ] `staged_is_ineligible_without_a_status_entry`
  - [ ] `staged_is_ineligible_for_an_untracked_file`
  - [ ] `pair_only_removed`
  - [ ] `rendered_rows_counts_binary_placeholder`
  - [ ] `pair_only_added`
  - [ ] `pair_equal_run_replace`
  - [ ] `pair_unequal_replace_more_added`
  - [ ] `pair_unequal_replace_more_removed`
  - [ ] `pair_interleaved_context`
  - [ ] `compute_hunk_tokens_returns_some_for_known_language`
  - [ ] `syntax_for_path_returns_none_for_unknown`
  - [ ] `snap_char_boundary_handles_multibyte`
  - [ ] `compute_hunk_tokens_dual_pure_added`
  - [ ] `compute_hunk_tokens_dual_pure_removed`

**`frontend/src/components/file_view.rs`**

- mod `conversion_tests` (3 tests):
  - [ ] `ascii_columns_map_to_themselves`
  - [ ] `cjk_three_byte_one_utf16_unit`
  - [ ] `astral_char_two_utf16_units`

**`frontend/src/components/project_rail.rs`**

- mod `tests` (5 tests):
  - [ ] `camel_case_uses_capitals`
  - [ ] `multi_token_uses_initials`
  - [ ] `single_lowercase_uses_prefix`
  - [ ] `single_capital_falls_back_to_prefix`
  - [ ] `empty_returns_placeholder`

**`frontend/src/components/sessions_panel.rs`**

- mod `tests` (9 tests):
  - [ ] `count_badge_does_not_overstate_the_counter`
  - [ ] `defaults_for_home_shows_other_projects_true`
  - [ ] `defaults_for_specific_project_shows_other_projects_false`
  - [ ] `defaults_hide_child_sessions_by_default`
  - [ ] `child_sessions_hidden_unless_toggled_on`
  - [ ] `show_other_projects_off_on_home_keeps_only_none_project`
  - [ ] `show_other_projects_off_in_project_requires_host_and_project_match`
  - [ ] `show_other_projects_on_bypasses_project_check`
  - [ ] `search_matches_alias_workspace_and_backend_case_insensitively`

**`frontend/src/components/settings_panel.rs`**

- mod `diff_pref_tests` (13 tests):
  - [ ] `diff_view_mode_roundtrip`
  - [ ] `diff_context_mode_roundtrip`
  - [ ] `diff_view_mode_unknown_is_none`
  - [ ] `diff_context_mode_unknown_is_none`
  - [ ] `tool_output_mode_roundtrip`
  - [ ] `tool_output_mode_unknown_is_none`
  - [ ] `broker_url_validator_accepts_empty`
  - [ ] `broker_url_validator_accepts_loopback_hosts`
  - [ ] `broker_url_validator_rejects_non_loopback_custom_broker`
  - [ ] `broker_url_validator_rejects_insecure_schemes`
  - [ ] `broker_url_validator_rejects_unknown_or_missing_scheme`
  - [ ] `broker_url_validator_rejects_embedded_credentials`
  - [ ] `broker_url_validator_rejects_fragments`

**`frontend/src/components/task_list.rs`**

- mod `native_tests` (5 tests):
  - [ ] `exact_context_without_matching_estimate_stays_neutral`
  - [ ] `categories_partition_the_bar_without_inferring_a_remainder`
  - [ ] `empty_breakdown_attributes_nothing_to_a_real_category`
  - [ ] `a_single_reported_category_is_rendered_as_reported`
  - [ ] `overfull_context_stays_within_the_bar`

**`frontend/src/components/tool_card/ask_user_question.rs`**

- mod `tests` (7 tests):
  - [ ] `format_answer_single_select_uses_header`
  - [ ] `format_answer_multi_select_joins_with_comma`
  - [ ] `format_answer_appends_custom_text`
  - [ ] `format_answer_custom_only`
  - [ ] `format_answer_falls_back_to_question_without_header`
  - [ ] `format_answer_multiple_questions_one_line_each`
  - [ ] `format_answer_skips_unanswered_questions`

**`frontend/src/components/tool_card/mod.rs`**

- mod `tool_visibility_tests` (2 tests):
  - [ ] `collapsed_large_lists_keep_successful_ask_questions_visible`
  - [ ] `collapsed_large_lists_keep_successful_exit_plan_mode_visible`
- mod `completion_summary_tests` (12 tests):
  - [ ] `modify_file_summary`
  - [ ] `run_command_summary_includes_streams`
  - [ ] `run_command_summary_no_streams_omits_them`
  - [ ] `run_command_summary_nonzero_exit`
  - [ ] `read_files_single_shows_bytes`
  - [ ] `read_files_multi_shows_count_and_total`
  - [ ] `search_types_zero`
  - [ ] `search_types_singular_vs_plural`
  - [ ] `get_type_docs_summary`
  - [ ] `error_summary_truncates_long_messages`
  - [ ] `error_summary_short_passes_through`
  - [ ] `count_summary_lines_handles_blank_and_text`
- mod `persistent_agent_resolver_tests` (6 tests):
  - [ ] `exact_id_ignores_stale_parent_host_and_uses_live_host`
  - [ ] `duplicate_exact_ids_use_only_parent_lineage`
  - [ ] `member_id_precedes_name_during_churn`
  - [ ] `name_alias_is_exact_direct_child_only`
  - [ ] `alias_duplicates_are_ambiguous_and_absent_parent_is_unavailable`
  - [ ] `any_exact_id_evidence_blocks_weaker_alias_fallback`

**`frontend/src/components/tool_card/modify_file.rs`**

- mod `tests` (3 tests):
  - [ ] `empty_diff_has_no_lines`
  - [ ] `single_added_line_classifies_correctly`
  - [ ] `replace_classifies_added_and_removed`

**`frontend/src/components/tool_card/run_command.rs`**

- mod `tests` (2 tests):
  - [ ] `truncate_short_passes_through`
  - [ ] `truncate_caps_at_line_cap`

**`frontend/src/dispatch.rs`**

- mod `tests` (66 tests):
  - [ ] `hover_result_uses_seeded_occurrence_without_active_project_lookup`
  - [ ] `references_context_is_bound_to_exact_initiating_resource`
  - [ ] `new_chat_before_bootstrap_survives_a_late_restore`
  - [ ] `chat_restore_refuses_a_project_mismatch`
  - [ ] `one_bootstrap_restores_project_chat_and_draft_together`
  - [ ] `chat_restore_refuses_a_session_mismatch`
  - [ ] `spawning_after_a_host_switch_cannot_carry_the_old_hosts_selection`
  - [ ] `a_draft_bound_to_the_spawn_target_is_left_alone`
  - [ ] `a_restored_draft_is_dropped_when_the_context_moves_to_another_host`
  - [ ] `draft_selection_is_not_applied_to_a_different_host`
  - [ ] `draft_selection_is_dropped_when_the_host_no_longer_offers_it`
  - [ ] `auto_force_upgrade_allows_managed_incompatible_once`
  - [ ] `auto_force_upgrade_rejects_already_attempted`
  - [ ] `auto_force_upgrade_rejects_non_managed_hosts`
  - [ ] `auto_force_upgrade_rejects_invalid_handshake`
  - [ ] `replayed_project_bootstrap_after_host_reconnect_is_accepted`
  - [ ] `file_list_preserves_same_relative_path_in_different_roots`
  - [ ] `unanswered_user_message_keeps_registered_agent_active_until_terminal_signal`
  - [ ] `terminal_typing_status_clears_stale_stream_state`
  - [ ] `task_replay_state_uses_final_genuine_update`
  - [ ] `fatal_agent_error_settles_desktop_ui_and_nonfatal_does_not`
  - [ ] `fatal_bootstrap_cannot_rearm_desktop_turn`
  - [ ] `malformed_call_tool_card_attaches_to_the_issuing_message_not_the_error_row`
  - [ ] `malformed_call_completion_lands_on_the_issuing_row`
  - [ ] `well_formed_calls_all_attach_to_their_issuing_message_in_order`
  - [ ] `undeclared_tool_call_still_falls_back_to_the_last_row`
  - [ ] `background_tool_completion_updates_prior_row_during_later_stream`
  - [ ] `stream_end_then_metadata_updated_patches_existing_row`
  - [ ] `agent_bootstrap_mid_turn_keeps_live_stream_end_visible`
  - [ ] `idle_bootstrap_overrides_replayed_user_message_activity`
  - [ ] `session_history_prepends_newest_first_page`
  - [ ] `session_history_uses_payload_owner_and_server_order`
  - [ ] `session_history_on_mismatched_stream_is_rejected`
  - [ ] `code_intel_error_frame_records_file_error`
  - [ ] `file_list_remove_is_scoped_to_root`
  - [ ] `agent_closed_cleans_exact_current_and_remembered_occurrence_state`
  - [ ] `server_driven_tab_upgrade_does_not_move_pane_focus`
  - [ ] `new_agent_keeps_submitted_settings_until_authoritative_snapshot`
  - [ ] `team_member_upgrade_does_not_move_pane_focus`
  - [ ] `new_agent_without_exact_draft_intent_opens_no_tab`
  - [ ] `inactive_project_draft_upgrade_mutates_same_tab_without_selection_changes`
  - [ ] `ambiguous_team_member_intent_is_not_guessed`
  - [ ] `context_compaction_marker_appends_one_inert_row`
  - [ ] `repeated_marker_id_materializes_exactly_one_row`
  - [ ] `richer_repeat_updates_the_marker_row_in_place`
  - [ ] `terminal_notify_clears_the_operation_without_a_replacement_agent`
  - [ ] `late_progress_for_a_terminal_operation_is_ignored`
  - [ ] `no_marker_terminates_a_running_operation`
  - [ ] `reconnect_ordering_keeps_the_operation_until_the_terminal_notify`
  - [ ] `bootstrap_omission_clears_stale_operation_and_capability`
  - [ ] `capability_for_a_foreign_session_is_rejected_and_fails_closed`
  - [ ] `duplicate_markers_within_one_page_produce_one_row`
  - [ ] `richer_paged_marker_merges_into_the_row_already_on_screen`
  - [ ] `typed_team_notify_fans_results_into_per_agent_operations`
  - [ ] `team_member_result_for_a_changed_session_is_skipped`
  - [ ] `a_team_run_announces_once_in_aggregate`
  - [ ] `notify_for_a_foreign_session_is_dropped`
  - [ ] `a_failed_marker_sentence_carries_its_token_metrics`
  - [ ] `a_structured_outcome_announces_exactly_once`
  - [ ] `an_observation_marker_announces_because_no_notify_will`
  - [ ] `bootstrap_restores_the_banner_without_announcing`
  - [ ] `failed_notify_records_a_reason_and_never_starts_a_fallback`
  - [ ] `history_replay_and_live_reducer_agree_on_marker_rows`
  - [ ] `stale_history_page_is_rejected_by_request_correlation`
  - [ ] `paged_marker_already_on_screen_is_not_duplicated`
  - [ ] `bootstrap_rebuilds_markers_once_and_restores_a_running_operation`
- mod `restore_fixtures` (test-only, no test fns)

**`frontend/src/line_source.rs`**

- mod `tests` (9 tests):
  - [ ] `file_lines_basic`
  - [ ] `file_lines_trailing_newline`
  - [ ] `file_lines_empty`
  - [ ] `file_lines_single_line`
  - [ ] `line_source_owned_dispatch`
  - [ ] `line_source_file_dispatch`
  - [ ] `line_start_and_content_end`
  - [ ] `line_for_byte_maps_offsets_to_lines`
  - [ ] `line_for_byte_empty_file`

**`frontend/src/markdown.rs`**

- mod `tests` (17 tests):
  - [ ] `fenced_info_string_with_modifier_still_highlights_as_rust`
  - [ ] `unknown_language_falls_back_to_escaped_text`
  - [ ] `no_language_renders_as_plain_text`
  - [ ] `javascript_link_is_unwrapped_to_plain_text`
  - [ ] `data_image_is_dropped_to_alt_text`
  - [ ] `http_link_is_preserved`
  - [ ] `https_and_mailto_and_relative_and_anchor_are_safe`
  - [ ] `dangerous_schemes_are_rejected_even_when_obfuscated`
  - [ ] `bare_url_is_autolinked`
  - [ ] `bare_url_mid_sentence_sheds_trailing_punctuation`
  - [ ] `bare_url_paren_handling_follows_gfm`
  - [ ] `url_with_underscores_is_linked_whole`
  - [ ] `urls_in_code_are_not_autolinked`
  - [ ] `url_as_existing_link_text_is_not_double_linked`
  - [ ] `url_in_image_alt_is_not_linkified`
  - [ ] `scheme_glued_to_a_word_is_left_alone`
  - [ ] `http_image_is_preserved`

**`frontend/src/state.rs`**

- mod `code_intel_tests` (10 tests):
  - [ ] `frame_at_rendered_version_is_applied`
  - [ ] `older_frame_is_dropped`
  - [ ] `newer_frame_is_stashed_then_applied_when_contents_arrive`
  - [ ] `frame_before_any_contents_is_stashed`
  - [ ] `pre_content_version_stash_is_bounded`
  - [ ] `version_change_drops_stale_decorations_and_ignores_late_old_frames`
  - [ ] `diagnostics_at_returns_hits_under_offset_most_severe_first`
  - [ ] `diagnostics_at_matches_zero_width_ranges_at_their_anchor`
  - [ ] `merge_model_merges_occurrences_by_range`
  - [ ] `merge_accumulates_byte_range_chunks_then_completes`
- mod `tests` (69 tests):
  - [ ] `upgrade_guard_starts_absent_then_marks_and_clears`
  - [ ] `upgrade_guard_clear_of_absent_id_is_noop`
  - [ ] `upgrade_guard_is_independent_per_host`
  - [ ] `close_others_keeps_target_and_non_closeable`
  - [ ] `close_to_right_removes_closeable_tabs_after_target`
  - [ ] `close_all_keeps_only_non_closeable`
  - [ ] `bump_tab_lru_pushes_to_front_dedup_truncate`
  - [ ] `forget_tab_lru_drops_only_target`
  - [ ] `prune_tab_lru_drops_ids_not_in_center_zone`
  - [ ] `rename_tab_label_only_changes_target`
  - [ ] `reduce_diff_response_matching_mode_clears_pending`
  - [ ] `reduce_diff_response_rejects_stale_mode`
  - [ ] `reduce_diff_response_ignores_when_no_outstanding_request`
  - [ ] `for_request_preserves_files_when_mode_unchanged`
  - [ ] `for_request_clears_files_on_mode_change`
  - [ ] `for_request_with_no_previous_starts_empty_pending`
  - [ ] `close_other_tabs_cleans_backing_state`
  - [ ] `diffs_for_different_paths_open_separate_tabs`
  - [ ] `close_tabs_to_right_cleans_backing_state`
  - [ ] `close_other_tabs_invalid_id_is_noop`
  - [ ] `active_agent_is_derived_from_active_chat_tab`
  - [ ] `chat_context_prefers_active_project_over_settings_selected_host`
  - [ ] `chat_context_prefers_active_agent_over_active_project`
  - [ ] `clear_host_runtime_drops_chat_state_for_host_agents`
  - [ ] `clear_host_runtime_drops_backend_config_schemas_for_host`
  - [ ] `clear_host_runtime_drops_only_that_hosts_project_runtime_state`
  - [ ] `forgetting_project_memory_cleans_exact_occurrences_and_keeps_survivors`
  - [ ] `compaction_cleanup_forgets_removed_occurrences_in_current_and_memory`
  - [ ] `host_cleanup_forgets_removed_occurrences_in_current_and_memory`
  - [ ] `clear_host_runtime_closes_host_tabs_even_without_agent_record`
  - [ ] `close_tabs_to_right_invalid_id_is_noop`
  - [ ] `split_ratio_clamps_every_constructor_input`
  - [ ] `duplicate_file_eligibility_distinguishes_refusals_without_mutation`
  - [ ] `duplicate_file_result_duplicates_then_activates_existing_occurrence`
  - [ ] `file_may_have_one_occurrence_per_pane`
  - [ ] `unloaded_file_cannot_be_duplicated`
  - [ ] `chats_are_nonduplicable_across_panes`
  - [ ] `duplicate_occurrences_have_independent_scroll_state`
  - [ ] `closing_one_of_two_occurrences_keeps_contents_and_subscription`
  - [ ] `close_all_tabs_with_two_occurrences_releases_exactly_once`
  - [ ] `closing_file_in_one_project_keeps_same_path_code_intel_in_another`
  - [ ] `closing_a_file_tab_keeps_code_intel_a_diff_tab_still_holds`
  - [ ] `closing_the_last_holding_diff_tab_drops_the_subscription`
  - [ ] `closing_file_and_diff_tabs_together_still_tears_the_file_down`
  - [ ] `goto_line_and_offset_target_only_one_occurrence`
  - [ ] `current_file_occurrence_requires_exact_tab_resource_and_version`
  - [ ] `duplicate_occurrence_navigation_targets_only_the_requested_side`
  - [ ] `closing_hover_owner_clears_only_that_occurrence_context`
  - [ ] `user_open_supersedes_refresh_and_refresh_never_supersedes_open`
  - [ ] `pending_open_destination_survives_focus_change`
  - [ ] `active_agent_and_pending_member_follow_composer_owner`
  - [ ] `project_switch_round_trips_split_layout_and_ratio`
  - [ ] `active_agent_follows_chat_pane_when_file_pane_is_focused`
  - [ ] `move_to_other_pane_preserves_tab_and_scroll_state`
  - [ ] `split_tab_to_left_places_dragged_tab_in_primary_pane`
  - [ ] `reveal_tab_moves_pane_focus_but_set_active_tab_in_pane_does_not`
  - [ ] `move_conflict_returns_authoritative_reason_without_mutation`
  - [ ] `move_refusal_conversion_rejects_success_and_preserves_refusal_data`
  - [ ] `side_open_and_move_refusal_reasons_use_canonical_constants`
  - [ ] `agent_open_to_side_opens_moves_and_reveals_without_duplication`
  - [ ] `agent_open_to_side_moves_existing_chat_with_same_tab_id`
  - [ ] `agent_open_to_side_eligibility_is_non_mutating_and_matches_refusals`
  - [ ] `agent_open_to_side_refuses_sole_tab_and_cross_project`
  - [ ] `diff_open_to_side_opens_reveals_and_moves_the_exact_occurrence`
  - [ ] `diff_open_to_side_eligibility_matches_typed_refusals_without_mutation`
  - [ ] `mounted_tabs_pin_each_panes_active_tab`
  - [ ] `apply_chat_message_metadata_patches_existing_row_in_place`
  - [ ] `pending_agent_settings_cleanup_is_request_scoped_and_fifo`
  - [ ] `active_project_restore_waits_for_owning_host_and_respects_new_selection`

**`frontend/src/voice.rs`**

- mod `gate_tests` (4 tests):
  - [ ] `provider_detail_is_appended_to_the_error_message`
  - [ ] `target_resolution_reports_each_false_reason`
  - [ ] `voice_gate_reports_each_false_conjunct_and_available`
  - [ ] `downlink_admission_classifies_every_webview_outcome`

### frontend/tauri-shell

**`frontend/tauri-shell/src/host_uds.rs`**

- mod `tests` (1 tests):
  - [ ] `socket_contention_rejects_before_host_start`

**`frontend/tauri-shell/src/lib.rs`**

- mod `tests` (6 tests):
  - [ ] `external_url_validation_allows_web_and_mail_links`
  - [ ] `external_url_validation_rejects_unsafe_or_internal_targets`
  - [ ] `navigation_guard_opens_only_external_urls`
  - [ ] `navigation_guard_keeps_configured_dev_server_in_the_webview`
  - [ ] `navigation_guard_does_not_whitelist_other_origins`
  - [ ] `navigation_guard_without_dev_server_treats_loopback_as_external`
- mod `native_voice_support_tests` (1 tests):
  - [ ] `native_voice_support_is_target_specific`
- mod `voice_media_command_contract_tests` (1 tests):
  - [ ] `production_handler_dispatches_camel_case_output_and_rejects_snake_case`
- mod `web_content_recovery_tests` (8 tests):
  - [ ] `termination_reloads_once_until_the_new_page_is_ready`
  - [ ] `ready_and_load_finished_rearm_in_either_order`
  - [ ] `deadline_notice_is_non_terminal_and_reload_error_is_observed_failure`
  - [ ] `readiness_deadline_restarts_after_hidden_recovery_becomes_visible`
  - [ ] `second_termination_escalates_now_or_on_the_next_observable_edge`
  - [ ] `recovery_dialog_escape_and_cancel_always_keep_waiting`
  - [ ] `rolling_attempt_limit_escalates_while_old_attempts_age_out`
  - [ ] `recovery_state_is_scoped_by_webview_label`

**`frontend/tauri-shell/src/main.rs`**

- mod `tests` (15 tests):
  - [ ] `defaults_to_gui_mode`
  - [ ] `ignores_macos_process_serial_number_argument`
  - [ ] `parses_host_stdio_subcommand`
  - [ ] `parses_host_uds_subcommand`
  - [ ] `parses_host_status_uds_subcommand`
  - [ ] `parses_host_launch_uds_subcommand`
  - [ ] `parses_host_bridge_uds_subcommand`
  - [ ] `parses_hermes_mcp_bridge_subcommand`
  - [ ] `parses_headless_stdio_alias`
  - [ ] `parses_headless_uds_alias`
  - [ ] `parses_headless_status_uds_alias`
  - [ ] `parses_headless_launch_uds_alias`
  - [ ] `parses_version_subcommand`
  - [ ] `parses_headless_bridge_uds_alias`
  - [ ] `rejects_incomplete_host_mode`

**`frontend/tauri-shell/src/remote_bootstrap.rs`**

- mod `tests` (4 tests):
  - [ ] `plans_lifecycle_actions_without_network`
  - [ ] `parses_remote_platform_aliases`
  - [ ] `selects_portable_assets`
  - [ ] `parses_version_path_last_component`

**`frontend/tauri-shell/src/router.rs`**

- mod `tests` (8 tests):
  - [ ] `trim_line_ending_strips_crlf`
  - [ ] `managed_bridge_uses_exact_target_binary_without_current_fallback`
  - [ ] `send_line_to_unknown_host_errors`
  - [ ] `send_line_rejects_embedded_newline`
  - [ ] `send_line_enqueues_to_writer_channel`
  - [ ] `send_line_errors_when_writer_gone`
  - [ ] `disconnect_unknown_host_errors`
  - [ ] `disconnect_clears_live_flag`

**`frontend/tauri-shell/src/voice_media.rs`**

- mod `tests` (18 tests):
  - [ ] `shell_handle_is_send_sync_without_audio_resources`
  - [ ] `threaded_lifecycle_requires_acceptance_and_acknowledges_fresh_drop`
  - [ ] `dropping_control_handle_shuts_thread_and_drops_session`
  - [ ] `media_command_queue_is_bounded_under_thread_backpressure`
  - [ ] `interrupt_barrier_invalidates_native_playback_callback_epoch`
  - [ ] `push_output_rejects_mismatched_generation_before_playback`
  - [ ] `native_processor_performs_real_echo_cancellation`
  - [ ] `select_f32_config_prefers_native_48k`
  - [ ] `select_f32_config_takes_closest_rate_for_bluetooth_style_mics`
  - [ ] `select_f32_config_blames_permission_only_when_formats_are_hidden`
  - [ ] `resampler_passthrough_is_bit_exact`
  - [ ] `resampler_upsamples_16k_to_48k_preserving_level_and_count`
  - [ ] `resampler_downsamples_48k_to_16k_preserving_level_and_count`
  - [ ] `resampler_tracks_fractional_ratios_without_drift`
  - [ ] `resampler_upsampling_interpolates_monotonic_ramps`
  - [ ] `jitter_gate_holds_silence_until_target_then_plays`
  - [ ] `jitter_gate_rearms_on_underrun_and_on_reset`
  - [ ] `resampler_reset_restarts_the_stream_clock`

### host-config

**`host-config/src/lib.rs`**

- mod `tests` (3 tests):
  - [ ] `release_versions_accept_prereleases`
  - [ ] `managed_lifecycle_accepts_legacy_release_field`
  - [ ] `host_line_delivery_id_is_backward_compatible`

### mobile-frontend

**`mobile-frontend/src/actions.rs`**

- mod `tests` (1 tests):
  - [ ] `project_and_review_stream_helpers_match_protocol_paths`

**`mobile-frontend/src/app.rs`**

- mod `native_tests` (1 tests):
  - [ ] `foreground_recovery_reconnects_only_after_the_grace_period`

**`mobile-frontend/src/bridge/web/connection.rs`**

- mod `tests` (26 tests):
  - [ ] `send_admission_is_typed_and_does_not_wait_for_writer`
  - [ ] `send_admission_rejects_full_dead_and_disconnected_queues`
  - [ ] `stop_control_bypasses_a_full_data_queue`
  - [ ] `typed_invalidation_is_not_user_stop_and_keeps_actor_for_reconnect`
  - [ ] `renewal_deadline_read_failure_is_retryable_and_invalidates_credentials`
  - [ ] `typed_transport_errors_survive_extra_io_wrapping`
  - [ ] `framing_corruption_stays_a_visible_non_retryable_failure`
  - [ ] `teardown_uses_dequeue_boundary_and_never_replays`
  - [ ] `continuous_inbound_and_data_pressure_cannot_delay_stop`
  - [ ] `frame_read_survives_a_write_completing_mid_record`
  - [ ] `completed_flush_acknowledges_exactly_one_logical_line`
  - [ ] `writer_deadline_cancels_session_without_resending_queue`
  - [ ] `same_active_data_room_replays_same_connection_instance_id`
  - [ ] `retry_reconnect_gets_new_connection_instance_id`
  - [ ] `terminal_statuses_have_no_connection_instance_id`
  - [ ] `needs_repair_is_terminal_and_repair_required`
  - [ ] `final_repair_failure_emits_the_typed_terminal_status_contract`
  - [ ] `unmanaged_custom_wss_record_fails_closed_with_repair`
  - [ ] `connect_timeout_is_retryable_rendezvous_wait_not_broker_failure`
  - [ ] `repeated_same_code_failures_become_persistent_and_reset_on_change`
  - [ ] `variable_error_detail_does_not_reset_persistence_counting`
  - [ ] `emit_persistent_failure_keeps_reconnecting_status`
  - [ ] `dropped_broker_is_retryable`
  - [ ] `not_authorized_retries_with_fresh_credentials`
  - [ ] `io_eof_carrying_transport_error_is_classified_by_transport`
  - [ ] `io_eof_carrying_publish_ack_mismatch_is_transport_failed`

**`mobile-frontend/src/bridge/web/mod.rs`**

- mod `tests` (5 tests):
  - [ ] `normalize_host_label_trims_and_rejects_empty`
  - [ ] `parse_and_validate_round_trips_a_valid_uri`
  - [ ] `parse_and_validate_accepts_https_fragment_pairing_uri`
  - [ ] `parse_and_validate_rejects_non_pairing_uri`
  - [ ] `parse_and_validate_rejects_protocol_mismatch`

**`mobile-frontend/src/bridge/web/qr.rs`**

- mod `tests` (1 tests):
  - [ ] `mobile_jsqr_copy_matches_loader_source`

**`mobile-frontend/src/bridge/web/service.rs`**

- mod `tests` (11 tests):
  - [ ] `hmac_sha256_matches_rfc4231_vector`
  - [ ] `pairing_id_is_extracted_from_path`
  - [ ] `auth_error_maps_documented_codes`
  - [ ] `redeem_error_maps_documented_codes`
  - [ ] `mint_error_maps_documented_codes`
  - [ ] `undocumented_code_is_surfaced_not_special_cased`
  - [ ] `cached_grant_connectable_respects_service_boundary_and_skew`
  - [ ] `contract_credentials_require_connect_valid_until_ms`
  - [ ] `service_contract_preserves_managed_websocket_url`
  - [ ] `service_contract_accepts_current_tycode_grant_query`
  - [ ] `service_contract_rejects_missing_or_invalid_websocket_url`

**`mobile-frontend/src/bridge/web/store.rs`**

- mod `tests` (3 tests):
  - [ ] `record_json_round_trips_and_omits_psk_bytes`
  - [ ] `fingerprint_matches_native_shape`
  - [ ] `summary_redacts_broker_password`

**`mobile-frontend/src/components/settings_view.rs`**

- mod `tool_output_mode_tests` (2 tests):
  - [ ] `roundtrip`
  - [ ] `unknown_is_none`

**`mobile-frontend/src/components/tool_card.rs`**

- mod `tests` (2 tests):
  - [ ] `format_answer_joins_selected_and_custom`
  - [ ] `format_answer_falls_back_to_question_without_header`

**`mobile-frontend/src/dispatch.rs`**

- mod `tests` (14 tests):
  - [ ] `context_compaction_marker_is_positioned_and_deduplicated`
  - [ ] `stale_history_page_is_rejected_by_request_id_and_cursor`
  - [ ] `compaction_bootstrap_restores_nonterminal_status_capability_and_marker`
  - [ ] `compaction_notifies_are_correlated_to_session_and_operation`
  - [ ] `agent_start_rejects_logical_session_rotation_without_clearing_state`
  - [ ] `conflicting_bootstrap_compaction_sessions_are_rejected`
  - [ ] `team_member_results_use_the_member_logical_session`
  - [ ] `fatal_agent_error_settles_the_turn_and_nonfatal_leaves_it_running`
  - [ ] `fatal_agent_error_does_not_touch_a_same_named_agent_on_another_host`
  - [ ] `fatal_bootstrap_cannot_rearm_the_turn`
  - [ ] `bootstrap_for_an_already_fatal_agent_stays_settled`
  - [ ] `agent_bootstrap_mid_turn_keeps_live_stream_end_visible`
  - [ ] `session_history_uses_payload_owner_and_server_order`
  - [ ] `background_tool_completion_updates_prior_message_during_later_stream`
- use `crate` (test-only, no test fns)

**`mobile-frontend/src/markdown.rs`**

- mod `tests` (7 tests):
  - [ ] `raw_block_html_is_downgraded_to_text`
  - [ ] `raw_inline_html_is_downgraded_to_text`
  - [ ] `javascript_link_is_unwrapped_to_plain_text`
  - [ ] `data_image_is_dropped_to_alt_text`
  - [ ] `safe_links_and_images_are_preserved`
  - [ ] `scheme_filter_matches_the_desktop_contract`
  - [ ] `ordinary_markdown_still_renders`

**`mobile-frontend/src/state.rs`**

- mod `tests` (22 tests):
  - [ ] `sort_project_infos_groups_workbenches_under_parent`
  - [ ] `sort_project_infos_orders_children_per_parent`
  - [ ] `sort_project_infos_pushes_orphan_workbenches_to_end`
  - [ ] `sort_project_infos_keeps_hosts_separate`
  - [ ] `local_host_id_serializes_transparent`
  - [ ] `paired_host_connection_status_maps_to_connection_status`
  - [ ] `broker_acknowledged_retires_the_record_silently`
  - [ ] `queued_submissions_are_held_but_never_surfaced`
  - [ ] `transport_failures_surface_with_their_typed_state`
  - [ ] `not_sent_surfaces_for_recovery_and_never_injects_into_the_composer`
  - [ ] `an_outcome_never_overwrites_a_newer_draft`
  - [ ] `new_chat_recovery_is_host_scoped_and_never_attributed_to_an_agent`
  - [ ] `an_outcome_from_a_foreign_connection_instance_is_ignored`
  - [ ] `a_withdrawn_submission_is_never_reported_as_still_queued`
  - [ ] `a_withdrawal_is_never_evicted_by_a_later_one`
  - [ ] `forgetting_a_host_drops_its_withdrawal_tombstones_and_only_its_own`
  - [ ] `a_broker_ack_leaves_the_lifecycle_at_queued_locally_and_claims_nothing_more`
  - [ ] `retiring_a_superseded_attempt_does_not_withdraw_the_message`
  - [ ] `wire_images_maps_empty_to_absent_and_non_empty_to_a_list`
  - [ ] `resend_is_only_offered_on_a_genuinely_new_connection`
  - [ ] `the_cap_refuses_new_submissions_and_never_evicts_an_unresolved_one`
  - [ ] `update_required_message_names_host_build_when_present`

### mobile-shell-types

**`mobile-shell-types/src/lib.rs`**

- mod `tests` (4 tests):
  - [ ] `local_host_id_round_trips_as_transparent_string`
  - [ ] `failed_status_uses_protocol_error_code_shape`
  - [ ] `connection_status_event_carries_instance_id`
  - [ ] `broker_auth_summary_does_not_serialize_password`

### mqtt-transport

**`mqtt-transport/src/chunking.rs`**

- mod `tests` (1 tests):
  - [ ] `chunks_at_64_kib`

**`mqtt-transport/src/client.rs`**

- mod `tests` (19 tests):
  - [ ] `duplicate_same_salt_after_session_is_ignored`
  - [ ] `different_salt_after_session_fails`
  - [ ] `missing_established_salt_after_session_fails`
  - [ ] `default_tls_roots_include_static_webpki_roots`
  - [ ] `real_broker_happy_path`
  - [ ] `ephemeral_connections_share_main_room_without_cross_talk`
  - [ ] `buffered_small_writes_are_boxcarred_on_flush`
  - [ ] `client_retries_handshake_until_delayed_host_subscribes`
  - [ ] `duplicate_client_handshake_after_ready_preserves_data_stream`
  - [ ] `different_client_handshake_after_ready_fails_stream`
  - [ ] `valid_pre_ready_data_frame_is_delivered_after_session_key`
  - [ ] `invalid_pre_ready_data_frame_fails_after_session_key`
  - [ ] `real_broker_wrong_psk_fails_with_aead`
  - [ ] `real_broker_chunking_transparent_for_one_megabyte`
  - [ ] `real_broker_cross_room_misroute_fails_aead`
  - [ ] `insecure_url_rejected`
  - [ ] `mqtt5_connection_rejection_is_surfaced`
  - [ ] `mqtt5_connection_reason_code_display_is_preserved`
  - [ ] `mqtt5_options_advertise_qos1_inflight_window`
- use `std` (test-only, no test fns)
- use `std` (test-only, no test fns)
- use `tokio` (test-only, no test fns)
- use `crate` (test-only, no test fns)
- use `crate` (test-only, no test fns)

**`mqtt-transport/src/config.rs`**

- mod `tests` (19 tests):
  - [ ] `managed_session_renews_before_expiry_and_immediately_when_late`
  - [ ] `managed_connection_plan_preserves_exact_client_id_and_scoped_topics`
  - [ ] `legacy_connection_plan_generates_an_identity_for_each_link`
  - [ ] `managed_connection_plan_accepts_exact_room_scoped_filters`
  - [ ] `managed_ephemeral_connection_plan_accepts_only_room_wildcard_filters`
  - [ ] `managed_ephemeral_connection_plan_rejects_exact_room_scoped_filters`
  - [ ] `managed_ephemeral_connection_plan_rejects_extra_broad_or_wrong_direction_filters`
  - [ ] `managed_connection_plan_rejects_exact_filter_for_wrong_room`
  - [ ] `managed_connection_plan_rejects_role_direction_mismatch`
  - [ ] `managed_connection_plan_rejects_client_id_role_mismatch`
  - [ ] `managed_connection_plan_rejects_client_id_grant_mismatch`
  - [ ] `managed_connection_plan_rejects_extra_or_broad_topic_filters`
  - [ ] `managed_connection_plan_rejects_missing_auth_material`
  - [ ] `managed_connection_plan_accepts_websocket_url_only_auth_material`
  - [ ] `managed_connection_plan_accepts_tycode_grant_query_without_token_key_name`
  - [ ] `managed_connection_plan_rejects_invalid_websocket_url_semantics`
  - [ ] `managed_connection_plan_redacts_token_from_endpoint_and_websocket_errors`
  - [ ] `managed_transports_select_only_service_issued_websocket_upgrade_auth`
  - [ ] `browser_managed_connect_packet_omits_mqtt_username_password`

**`mqtt-transport/src/error.rs`**

- mod `tests` (1 tests):
  - [ ] `authorization_rejection_retries_with_fresh_credentials`

**`mqtt-transport/src/framing.rs`**

- mod `tests` (4 tests):
  - [ ] `handshake_round_trip`
  - [ ] `rejects_wrong_version`
  - [ ] `credit_round_trip`
  - [ ] `rejects_short_credit_frame`

**`mqtt-transport/src/link_native.rs`**

- mod `codec_parity_tests` (2 tests):
  - [ ] `mqttbytes_publish_round_trips_and_rumqttc_decodes_it`
  - [ ] `mqttbytes_subscribe_round_trips_and_rumqttc_decodes_it`
- use `std` (test-only, no test fns)
- use `std` (test-only, no test fns)

**`mqtt-transport/src/protocol_driver.rs`**

- mod `tests` (18 tests):
  - [ ] `host_ignores_an_open_from_an_older_transport_version`
  - [ ] `host_still_rejects_a_malformed_current_open`
  - [ ] `managed_session_deadline_closes_the_stream_for_credential_renewal`
  - [ ] `missing_puback_fails_the_stream_instead_of_blocking_writes`
  - [ ] `puback_progress_extends_the_stall_watchdog`
  - [ ] `loopback_brokers_are_not_subject_to_managed_service_pacing`
  - [ ] `outbound_budget_paces_data_publishes`
  - [ ] `data_pipelines_up_to_receiver_credit_window`
  - [ ] `receiver_emits_standalone_cumulative_credit`
  - [ ] `credit_puback_does_not_complete_data_write_ack`
  - [ ] `quota_rejected_data_publish_is_retried_with_same_counter`
  - [ ] `quota_rejected_credit_publish_is_retried_with_same_counter`
  - [ ] `one_way_bulk_transfer_crosses_credit_windows`
  - [ ] `credit_blocked_sender_fails_explicitly_without_credit`
  - [ ] `credit_publish_rejection_fails_stream`
  - [ ] `puback_rejection_fails_all_inflight_data_and_closes`
  - [ ] `disconnect_fails_all_inflight_data_and_closes`
  - [ ] `wasm_unknown_puback_mismatch_fails_all_inflight_flush_acks`
- use `std` (test-only, no test fns)
- use `tokio` (test-only, no test fns)

**`mqtt-transport/src/reconnect.rs`**

- mod `tests` (2 tests):
  - [ ] `rejects_invalid_backoff_configuration`
  - [ ] `base_delay_caps_at_max`

**`mqtt-transport/src/rendezvous.rs`**

- mod `tests` (3 tests):
  - [ ] `open_request_round_trip`
  - [ ] `accept_round_trip_and_ephemeral_key_match`
  - [ ] `repeated_control_frames_use_unique_nonces`

**`mqtt-transport/src/session.rs`**

- mod `tests` (16 tests):
  - [ ] `host_and_client_derive_matching_keys`
  - [ ] `delivers_in_order_and_drops_duplicates`
  - [ ] `counter_one_before_zero_buffers_and_drains`
  - [ ] `buffers_within_window_reorder_and_drains_in_order`
  - [ ] `receive_window_accepts_last_counter_before_boundary`
  - [ ] `counter_at_receive_window_boundary_is_fatal`
  - [ ] `counter_22_after_6_buffers_and_drains`
  - [ ] `missing_counters_7_to_11_withhold_12_to_22_until_gap_arrives`
  - [ ] `credit_round_trip_updates_peer_credit`
  - [ ] `duplicate_stale_and_lower_credit_are_noops`
  - [ ] `credit_control_counter_gaps_are_tolerated`
  - [ ] `future_credit_beyond_sent_is_fatal`
  - [ ] `wrong_credit_direction_fails_aead`
  - [ ] `tampered_credit_fails_aead`
  - [ ] `wrong_direction_byte_fails_aead`
  - [ ] `cross_room_aad_misroute_fails_aead`

**`mqtt-transport/src/stream.rs`**

- mod `tests` (2 tests):
  - [ ] `flush_waits_for_outbound_ack`
  - [ ] `production_stream_boundary_allows_only_typed_voice_audio`

**`mqtt-transport/src/topic.rs`**

- mod `tests` (3 tests):
  - [ ] `topic_round_trip`
  - [ ] `managed_topics_use_scoped_namespace_and_room_segment`
  - [ ] `managed_topics_reject_wildcard_namespace`

**`mqtt-transport/src/types.rs`**

- mod `tests` (19 tests):
  - [ ] `default_broker_endpoint_is_emqx_wss`
  - [ ] `pairing_qr_round_trips_mqtt_endpoint_policy_room_and_psk`
  - [ ] `pairing_qr_decodes_legacy_cbor_without_release_version`
  - [ ] `pairing_qr_round_trips_some_release_version_and_omits_none`
  - [ ] `pairing_url_round_trips_through_from_any`
  - [ ] `pairing_url_keeps_psk_only_in_fragment`
  - [ ] `from_any_accepts_legacy_and_https_forms`
  - [ ] `managed_pairing_qr_round_trips_in_v2_fragment_without_debug_secret_leak`
  - [ ] `qr_offer_classifies_legacy_public_broker_as_repair_required`
  - [ ] `supported_qr_entry_point_classifies_legacy_public_broker_as_repair_required`
  - [ ] `pairing_qr_version_mismatch_is_typed`
  - [ ] `managed_pairing_qr_fails_closed_for_broker_secrets_in_url`
  - [ ] `managed_pairing_qr_requires_release_version`
  - [ ] `managed_pairing_qr_requires_room_and_psk`
  - [ ] `managed_pairing_qr_rejects_unsupported_transport_version`
  - [ ] `managed_pairing_qr_rejects_empty_offer_id_from_cbor`
  - [ ] `pre_shared_key_debug_redacts_secret_material`
  - [ ] `broker_url_validation_rejects_plaintext_public_schemes`
  - [ ] `broker_url_validation_rejects_mqtts_path_and_embedded_credentials`

**`mqtt-transport/src/wasm_codec.rs`**

- mod `tests` (7 tests):
  - [ ] `qos1_publish_requires_a_matching_puback`
  - [ ] `qos0_publish_requires_no_ack`
  - [ ] `qos2_publish_is_a_protocol_error`
  - [ ] `looked_up_puback_token_is_accepted`
  - [ ] `quota_exceeded_puback_is_classified_for_pacing`
  - [ ] `matching_suback_pkid_is_accepted_mismatch_is_ignored`
  - [ ] `pingreq_and_disconnect_encode_to_expected_packet_types`

### protocol

**`protocol/src/framing.rs`**

- mod `tests` (6 tests):
  - [ ] `rejects_corruption_and_oversize_before_allocation`
  - [ ] `reassembles_large_json_and_preserves_binary`
  - [ ] `rejects_unknown_version_kind_flags_and_fragment_order`
  - [ ] `golden_v1_hello_record_remains_readable`
  - [ ] `golden_v1_reject_record_remains_readable`
  - [ ] `handshake_writer_still_emits_v1_records`

**`protocol/src/validator.rs`**

- mod `tests` (55 tests):
  - [ ] `rejects_context_compaction_notify_for_stale_logical_session`
  - [ ] `rejects_team_context_result_without_member_logical_session`
  - [ ] `missing_host_bootstrap_session_list_is_rejected`
  - [ ] `missing_session_list_page_is_rejected`
  - [ ] `session_summary_count_update_requires_typed_response_count_wire_field`
  - [ ] `session_summary_count_update_requires_authoritative_updated_at`
  - [ ] `host_bootstrap_after_welcome_registers_agent_streams`
  - [ ] `post_handshake_host_bootstrap_registers_agent_streams`
  - [ ] `live_backend_config_schemas_are_valid_host_frames`
  - [ ] `live_backend_config_snapshots_are_valid_host_frames`
  - [ ] `rejects_host_replay_before_host_bootstrap`
  - [ ] `rejects_agent_event_before_agent_bootstrap`
  - [ ] `rejects_project_event_before_project_bootstrap`
  - [ ] `accepts_workspace_review_summary_scope_payloads`
  - [ ] `accepts_team_member_origin_with_team_fields`
  - [ ] `rejects_team_member_origin_without_team_fields`
  - [ ] `rejects_non_team_origin_with_team_fields`
  - [ ] `accepts_workflow_origin_with_metadata`
  - [ ] `rejects_workflow_origin_without_metadata`
  - [ ] `rejects_non_workflow_origin_with_workflow_metadata`
  - [ ] `rejects_fork_spawn_with_parent_agent_id`
  - [ ] `rejects_fork_spawn_without_from_session_id`
  - [ ] `accepts_turn_with_tools_after_stream_end`
  - [ ] `warning_during_stream_does_not_close_assistant_turn`
  - [ ] `system_message_during_stream_does_not_close_assistant_turn`
  - [ ] `error_message_closes_assistant_turn`
  - [ ] `conversation_cleared_releases_known_and_terminal_stream_message_ids`
  - [ ] `operation_cancelled_discards_an_open_stream_for_the_next_stream_start`
  - [ ] `accepts_non_streaming_turn_with_tools_after_assistant_message`
  - [ ] `accepts_metadata_update_after_known_assistant_message_id`
  - [ ] `rejects_duplicate_same_sender_message_id`
  - [ ] `rejects_metadata_update_for_unknown_message_id`
  - [ ] `rejects_metadata_update_for_non_assistant_message_id`
  - [ ] `rejects_stream_end_message_id_mismatch`
  - [ ] `rejects_missing_and_foreign_stream_delta_ids_without_rebinding`
  - [ ] `rejects_second_stream_start_without_closing_or_rebinding_the_first`
  - [ ] `rejects_reused_terminal_stream_message_id`
  - [ ] `accepts_streaming_turn_with_tool_request_before_stream_end`
  - [ ] `rejects_tool_request_before_assistant_turn`
  - [ ] `rejects_stream_delta_before_stream_start`
  - [ ] `rejects_agent_activity_stats_on_host_stream`
  - [ ] `accepts_agent_activity_stats_on_agent_stream_after_start`
  - [ ] `accepts_next_message_while_tool_request_is_unresolved`
  - [ ] `accepts_assistant_message_added_while_tool_request_is_unresolved`
  - [ ] `operation_cancelled_clears_unresolved_tool_requests`
  - [ ] `accepts_late_tool_completion_after_operation_cancelled`
  - [ ] `rejects_unknown_tool_completion_even_after_assistant_turn`
  - [ ] `accepts_mixed_non_streaming_sequence_across_multiple_turns`
  - [ ] `accepts_unbounded_and_bounded_session_pages`
  - [ ] `rejects_session_page_advertising_more_without_a_limit`
  - [ ] `rejects_session_page_limits_outside_the_advertised_bound`
  - [ ] `voice_audio_requires_an_accepted_typed_session`
  - [ ] `host_voice_capabilities_require_current_bounded_values`

### server

**`server/src/acceptor.rs`**

- mod `handshake_contract_tests` (1 tests):
  - [ ] `version_mismatch_is_answered_with_a_reject_carrying_release_version`
- mod `tests` (2 tests):
  - [ ] `concurrent_uds_binding_has_exactly_one_owner`
  - [ ] `stale_uds_path_is_recovered_before_binding`

**`server/src/agent/customization.rs`**

- mod `tests` (10 tests):
  - [ ] `native_backends_carry_no_body_text_anywhere_in_the_resolved_config`
  - [ ] `native_resolution_does_not_open_an_unreadable_body`
  - [ ] `hermes_receives_names_without_reading_bodies`
  - [ ] `resolved_bodies_never_contradict_the_session_delivery`
  - [ ] `a_path_only_skill_can_never_report_inline_text`
  - [ ] `rendering_a_body_always_implies_loading_one`
  - [ ] `resolved_skill_directories_stay_inside_the_store`
  - [ ] `every_backend_states_the_skill_seam_it_actually_has`
  - [ ] `legacy_backends_still_receive_inline_bodies`
  - [ ] `explicit_custom_agent_resolves_only_its_selected_skills`

**`server/src/agent/mod.rs`**

- mod `tests` (112 tests):
  - [ ] `stream_identity_missing_end_id_recovers_without_poisoning_session`
  - [ ] `stream_identity_foreign_start_recovery_finalizes_abandoned_stream`
  - [ ] `stream_identity_ambiguous_shapes_stay_unrecoverable`
  - [ ] `actor_interrupt_parks_terminal_while_close_ends_startup`
  - [x] `pending_startup_attachments_receive_complete_bootstrap_before_live_events` — `server::agents startup_fanout_advertises_all_subscribers_before_any_attach_wait`
  - [ ] `supervisor_failure_warning_copy_pluralizes_typed_attempt_count`
  - [ ] `supervisor_failure_warning_actor_gate_dedupes_without_activity`
  - [ ] `supervisor_failure_warning_rejects_in_turn_and_queued_work`
  - [ ] `supervisor_failure_warning_samples_settings_after_status_await`
  - [x] `pending_startup_attachment_failure_receives_terminal_bootstrap` — `server::agents terminal_startup_failure_bootstraps_before_immediate_rejection`
  - [ ] `tycode_constructor_never_enters_runtime_or_replay_state`
  - [ ] `tycode_genuine_tasks_survive_store_reload_and_resume`
  - [ ] `tycode_legacy_unprovenanced_snapshot_is_not_migrated`
  - [ ] `runtime_session_updates_count_terminal_assistant_responses`
  - [ ] `activity_summary_tool_request_does_not_fail_when_text_arrives`
  - [ ] `activity_summary_tool_request_empty_stream_end_waits_for_later_text`
  - [ ] `activity_summary_reasoning_only_stream_end_waits_for_later_answer`
  - [ ] `activity_summary_tool_request_without_text_is_explicit_error`
  - [ ] `activity_stats_tracks_latest_output_without_stream_start_or_end_clearing`
  - [ ] `activity_stats_accumulates_reasoning_deltas`
  - [ ] `activity_stats_counts_unique_tool_requests`
  - [ ] `activity_stats_token_metadata_replaces_by_message_id`
  - [ ] `codex_activity_stats_use_model_requests_not_chat_records`
  - [ ] `codex_current_context_is_exact_persistent_and_compaction_scoped`
  - [ ] `activity_stats_stamps_cumulative_scope_without_changing_request_scope`
  - [ ] `activity_stats_does_not_synthesize_known_cumulative_from_incomplete_sources`
  - [ ] `activity_stats_preserves_request_only_usage_without_inventing_turn`
  - [ ] `claude_activity_stats_replace_provisional_requests_with_terminal_usage`
  - [ ] `claude_activity_stats_carry_unreconciled_requests_across_turn_reset_once`
  - [ ] `claude_terminal_turn_replaces_provisional_usage_but_keeps_real_partial_scope`
  - [ ] `activity_stats_preserves_explicit_ambiguous_cumulative_scope`
  - [ ] `usage_snapshot_empty_reports_no_completed_assistant_turn`
  - [ ] `usage_snapshot_all_unavailable_reports_unavailable_reason`
  - [ ] `usage_snapshot_mixed_known_and_unavailable_sources_is_partial`
  - [ ] `usage_snapshot_from_log_preserves_mixed_source_partial_usage`
  - [ ] `usage_snapshot_from_log_uses_latest_stats_total_as_numeric_floor`
  - [ ] `usage_snapshot_from_log_combines_visible_unavailable_with_stats_total`
  - [ ] `usage_snapshot_from_log_uses_stats_fallback_without_chat_usage`
  - [ ] `usage_snapshot_from_log_preserves_total_only_activity_snapshot`
  - [ ] `activity_stats_uses_event_seq_for_unidentified_token_usage`
  - [ ] `claude_total_only_usage_is_known_without_invented_components`
  - [ ] `relay_activity_stats_replace_native_total_only_usage`
  - [ ] `relay_preserves_codex_usage_across_typing_live_and_replay`
  - [x] `bootstrap_includes_current_activity_stats_snapshot` — `server::agents turn_token_usage_is_known_cumulative_and_bootstrapped`
  - [ ] `generated_name_sanitizer_accepts_valid_name`
  - [ ] `resumed_agent_start_is_idle_before_follow_up`
  - [ ] `resumed_history_bootstrap_ends_with_authoritative_idle`
  - [ ] `completed_bootstrap_carries_authoritative_idle_state`
  - [ ] `accepted_turn_marks_completed_agent_active`
  - [x] `pending_tool_response_keeps_backend_visibly_busy` — `server::queue exit_plan_mode_tool_response_resumes_and_drains_queue`
  - [ ] `relay_idle_marker_makes_native_child_inactive`
  - [ ] `generated_name_timeout_retains_fallback_without_agent_error`
  - [ ] `legacy_native_collaboration_replay_is_sanitized_but_other_tools_are_not`
  - [ ] `hidden_inference_setup_preserves_scoped_hermes_session_settings`
  - [ ] `compaction_marker_preserves_cumulative_token_and_task_usage`
  - [ ] `generated_name_waits_past_reasoning_only_stream_end`
  - [ ] `generated_name_ignores_setup_typing_cycle_before_turn`
  - [ ] `generated_name_empty_completion_fails_without_prompt_fallback`
  - [ ] `generated_name_accepts_single_word_answer`
  - [ ] `generated_name_sanitizer_truncates_overlong_answer`
  - [ ] `generated_name_sanitizer_rejects_answer_with_no_usable_words`
  - [ ] `mock_name_uses_default_words_when_prompt_has_no_name_words`
  - [ ] `bootstrap_tail_starts_at_message_boundary_with_tool_history`
  - [ ] `session_history_window_pages_by_message_boundary_with_tool_history`
  - [ ] `bootstrap_completed_stream_replays_post_end_tool_events_after_stream_end`
  - [ ] `bootstrap_replays_positioned_tool_between_pre_and_post_text`
  - [ ] `reconnect_persists_prior_tool_completion_outside_later_active_stream`
  - [ ] `bootstrap_completed_stream_replays_metadata_updated_stream_end`
  - [ ] `bootstrap_completed_stream_filter_keeps_later_history_entries`
  - [ ] `replay_compacts_completed_stream_into_message_added`
  - [ ] `replay_coalesces_tool_progress_latest_wins`
  - [ ] `bootstrap_replays_active_background_progress_outside_history_tail_once`
  - [ ] `output_events_since_returns_metadata_update_after_message_seq`
  - [ ] `latest_output_state_does_not_fall_back_past_empty_message`
  - [ ] `replay_preserves_active_stream_as_aggregated_deltas`
  - [ ] `replay_preserves_server_generated_identity_without_rederiving_it`
  - [ ] `replay_rejects_foreign_delta_without_rebinding_or_raw_id_leakage`
  - [ ] `gated_foreign_delta_is_rejected_before_stats_or_active_text_mutate`
  - [ ] `replay_rejects_duplicate_terminal_stream_identity`
  - [ ] `replay_rejects_duplicate_same_sender_message_identity`
  - [ ] `empty_stream_completion_remains_durable_after_idle`
  - [ ] `cancelled_reasoning_stream_remains_durable_with_its_stream_identity`
  - [ ] `live_and_replay_stream_frames_preserve_exact_message_identity`
  - [ ] `replay_active_reasoning_preserves_stream_message_id_before_live_end`
  - [ ] `replay_preserves_active_typing_before_stream_start`
  - [ ] `replay_preserves_completed_stream_until_idle`
  - [ ] `replay_keeps_stream_tool_events_after_message`
  - [ ] `failed_agent_actor_replays_terminal_error_and_rejects_input`
  - [ ] `deliver_message_rejects_a_parked_terminal_actor_without_touching_it`
  - [ ] `deliver_message_reports_a_dead_mailbox_and_a_closing_handle`
  - [ ] `deliver_message_rejects_a_relay_actor_without_touching_it`
  - [x] `deliver_message_queues_behind_an_open_turn_and_reports_active` — `server::queue queue_while_busy_snapshot_grows`
  - [x] `close_interrupts_a_held_turn_instead_of_waiting` — `server::agents close_agent_with_busy_subagents_completes_and_removes_the_subtree`
  - [ ] `close_completes_when_the_backend_ignores_the_interrupt`
  - [ ] `interrupt_during_close_interrupts_instead_of_reporting_not_running`
  - [ ] `input_arriving_after_close_reports_that_the_agent_is_closing`
  - [ ] `resume_barrier_keeps_an_acknowledged_gated_delivery_active`
  - [ ] `busy_redelivery_retains_private_queue_sequence`
  - [ ] `compaction_watermark_partitions_pre_and_post_request_messages`
  - [ ] `inline_fallback_requires_exact_rejected_not_observed_terminal`
  - [ ] `rejected_terminal_fallback_clears_native_acceptance_before_spawn`
  - [ ] `background_task_must_reach_terminal_progress_before_compaction_dispatch`
  - [ ] `deferred_compaction_retry_rearms_once_per_timer`
  - [ ] `automatic_only_backend_fallback_is_explicit_request_only`
  - [ ] `terminal_marker_retains_text_command_mechanism_from_flight`
  - [ ] `compaction_snapshot_replacement_preserves_single_envelope`
  - [ ] `backend_transcript_metadata_reaches_authoritative_record`
  - [ ] `context_compaction_notify_stamps_authoritative_current_session`
  - [ ] `rejected_not_observed_falls_back_once_in_the_same_operation`
  - [ ] `unknown_capability_is_rejected_before_admission`
  - [ ] `all_compaction_terminal_paths_release_input_barrier`
  - [ ] `internal_compaction_steps_do_not_change_supervision_user_fold`

**`server/src/agent/registry.rs`**

- mod `tests` (2 tests):
  - [ ] `status_derivation_separates_recoverable_error_from_fatal_failure`
  - [ ] `remove_agent_notifies_status_watch_only_after_success`

**`server/src/agent/supervisor.rs`**

- mod `tests` (22 tests):
  - [ ] `snapshot_tracks_user_and_assistant_messages`
  - [ ] `snapshot_tracks_latest_assistant_context_and_matching_metadata`
  - [ ] `snapshot_accepts_late_matching_context_metadata`
  - [ ] `snapshot_counts_kicks_and_resets_on_real_user_message`
  - [ ] `snapshot_pairs_each_kick_with_the_reply_it_drew`
  - [ ] `snapshot_tracks_errors_and_cancellation_since_user_message`
  - [ ] `snapshot_separates_a_stall_interrupt_from_a_user_cancel`
  - [ ] `snapshot_ignores_unrelated_warning_cards`
  - [ ] `stall_interrupt_prompt_reports_the_truncated_turn`
  - [ ] `parse_accepts_all_exact_verdicts`
  - [ ] `parse_tolerates_fences_and_markdown_decoration`
  - [ ] `parse_rejects_invalid_output`
  - [ ] `prompt_states_the_unintended_stop_contract`
  - [ ] `prompt_omits_the_repeat_section_on_a_first_verdict`
  - [ ] `prompt_preserves_the_complete_final_assistant_message`
  - [ ] `mock_sentinels_map_to_explicit_verdicts`
  - [ ] `supervisor_classifies_non_retryable_backend_failures`
  - [ ] `supervisor_hermes_terminal_taxonomy_fails_closed_without_parsing_prose`
  - [ ] `supervisor_spawn_preserves_scoped_hermes_session_settings`
  - [ ] `compaction_marker_changes_only_token_and_guard_fold_fields`
  - [ ] `non_mutating_failed_compaction_preserves_current_context_tokens`
  - [ ] `real_user_message_releases_both_compaction_guards`

**`server/src/agent_control_mcp.rs`**

- mod `tests` (24 tests):
  - [ ] `rejects_non_loopback_bind_addr`
  - [ ] `split_request_target_reads_percent_encoded_agent_id`
  - [ ] `split_request_target_rejects_invalid_agent_id`
  - [ ] `caller_credentials_bind_signature_to_agent_id`
  - [ ] `cap_read_events_advances_past_omitted_events`
  - [ ] `latest_agent_output_returns_only_visible_message_text`
  - [ ] `latest_agent_output_preserves_empty_and_error_records`
  - [ ] `read_tool_schemas_separate_latest_and_debug_inputs`
  - [ ] `latest_and_await_inputs_reject_legacy_fields`
  - [ ] `send_message_schema_requires_non_empty_fields`
  - [ ] `debug_input_accepts_incremental_controls`
  - [ ] `list_agents_returns_only_callers_direct_children`
  - [ ] `read_only_guidance_mode_allows_child_spawns`
  - [ ] `caller_cannot_assign_a_different_parent`
  - [ ] `spawn_agent_accepts_explicit_hermes_launch_profile`
  - [ ] `await_agents_does_not_return_while_still_thinking`
  - [ ] `await_agents_remains_pending_beyond_prior_300_second_boundary`
  - [ ] `await_agents_fails_on_request_cancellation`
  - [ ] `blocked_await_wakes_on_fatal_backend_closure`
  - [ ] `send_agent_message_commits_actor_status_before_success`
  - [ ] `rejected_terminal_delivery_preserves_failed_status_and_output`
  - [ ] `rejected_blocked_compaction_delivery_remains_ready_idle`
  - [ ] `await_snapshot_treats_removed_watched_agent_as_ready_idle`
  - [ ] `team_describe_binding_rejects_missing_member_binding`

**`server/src/backend/claude.rs`**

- mod `tests` (1 tests):
  - [ ] `completion_without_pending_request_is_refused`

**`server/src/backend/codex.rs`**

- mod `response_splitter_tests` (21 tests):
  - [ ] `captured_multi_tool_response_stays_one_message`
  - [ ] `captured_two_responses_in_one_turn_become_two_messages`
  - [ ] `interleaved_root_and_child_responses_are_partitioned_by_thread`
  - [ ] `late_tool_completion_keeps_closed_response_owner`
  - [ ] `visible_retry_closes_provisional_response`
  - [ ] `response_id_proves_an_empty_provider_response`
  - [ ] `tool_request_without_provider_evidence_does_not_open_a_message`
  - [ ] `raw_custom_tool_output_resolves_the_provider_call_owner`
  - [ ] `captured_typed_and_raw_tool_views_are_deduplicated`
  - [ ] `raw_tool_owner_eviction_is_ordered_and_visible`
  - [ ] `retry_notification_terminalizes_incomplete_tool`
  - [ ] `turn_completion_without_raw_completion_terminalizes_tool`
  - [ ] `transport_loss_terminalizes_incomplete_tool`
  - [ ] `strict_unified_exec_promotes_before_raw_completion`
  - [ ] `strict_child_unified_exec_preserves_execution_lifecycle`
  - [ ] `strict_parallel_forwarded_sessions_match_call_identity`
  - [ ] `strict_parallel_unlinked_nested_execs_fail_without_guessing`
  - [ ] `strict_non_yielding_nested_exec_keeps_plain_success`
  - [ ] `cancel_after_intermediate_text_and_tool_response_interrupts_active_turn`
  - [ ] `strict_image_completion_carries_to_next_response`
  - [ ] `raw_event_warning_is_legacy_only`

**`server/src/backend/turn_emitter.rs`**

- mod `tests` (11 tests):
  - [ ] `undeclared_tool_request_is_refused_without_fabricating_a_message`
  - [ ] `claude_declared_tool_sequence_keeps_provider_message_identity`
  - [ ] `warning_during_stream_preserves_assistant_turn_on_wire`
  - [ ] `system_message_during_stream_preserves_assistant_turn_on_wire`
  - [ ] `error_message_closes_assistant_turn_on_wire`
  - [ ] `synthetic_container_close_restores_closed_assistant_turn`
  - [ ] `synthetic_container_close_restores_open_assistant_turn`
  - [ ] `user_clear_during_open_container_survives_close`
  - [ ] `error_clear_during_open_container_survives_close`
  - [ ] `real_assistant_open_during_saved_false_container_survives_close`
  - [ ] `detached_tool_owner_survives_turn_reset_until_completion`

**`server/src/browse_stream.rs`**

- mod `tests` (4 tests):
  - [ ] `list_dir_follows_directory_symlink`
  - [ ] `project_roots_initial_path_uses_single_root_directly`
  - [ ] `project_roots_initial_path_uses_deepest_common_parent`
  - [ ] `project_roots_initial_path_returns_none_without_useful_parent`

**`server/src/code_intel/bootstrap.rs`**

- mod `tests` (10 tests):
  - [ ] `prefers_path_over_rustup`
  - [ ] `falls_back_to_rustup_when_not_on_path`
  - [ ] `rejects_broken_path_candidate_and_uses_rustup_candidate`
  - [ ] `absent_with_install_hint_when_neither_found`
  - [ ] `absent_includes_probe_failure_for_broken_path_candidate`
  - [ ] `looks_up_the_right_binary_name`
  - [ ] `configured_path_takes_precedence_without_path_or_rustup_lookup`
  - [ ] `invalid_configured_path_fails_without_fallback_or_rustup_hint`
  - [ ] `custom_toolchain_proxy_failure_uses_custom_hint`
  - [ ] `official_toolchain_missing_component_keeps_rustup_hint`

**`server/src/code_intel/lsp_client.rs`**

- mod `tests` (8 tests):
  - [ ] `correlates_responses_and_delivers_notifications`
  - [ ] `malformed_traffic_surfaces_as_protocol_error_event`
  - [ ] `pending_requests_fail_when_server_disconnects`
  - [ ] `unanswered_request_times_out_and_cleans_pending_entry`
  - [ ] `teardown_reaps_child_no_zombie`
  - [ ] `exited_child_surfaces_late_stderr_and_status`
  - [ ] `stderr_tail_is_bounded_for_newline_less_output`
  - [ ] `drop_after_server_exit_reaps_lingering_process_group_child`

**`server/src/code_intel/lsp_codec.rs`**

- mod `tests` (15 tests):
  - [ ] `round_trip_single_message`
  - [ ] `header_includes_byte_length_not_char_length`
  - [ ] `partial_header_then_rest`
  - [ ] `partial_body_arrives_in_pieces`
  - [ ] `one_byte_at_a_time`
  - [ ] `multiple_messages_in_one_buffer`
  - [ ] `extra_headers_are_ignored`
  - [ ] `content_length_is_case_insensitive`
  - [ ] `missing_content_length_is_an_error`
  - [ ] `invalid_content_length_is_an_error`
  - [ ] `oversize_content_length_is_rejected_not_buffered`
  - [ ] `content_length_that_would_overflow_is_rejected`
  - [ ] `header_with_no_terminator_past_cap_is_rejected`
  - [ ] `partial_header_under_cap_still_waits`
  - [ ] `invalid_json_body_is_an_error_but_buffer_advances`

**`server/src/code_intel/lsp_position.rs`**

- mod `tests` (13 tests):
  - [ ] `ascii_single_line`
  - [ ] `ascii_multi_line`
  - [ ] `emoji_astral_plane_two_utf16_units`
  - [ ] `cjk_three_byte_one_utf16_unit`
  - [ ] `combining_marks`
  - [ ] `crlf_line_endings`
  - [ ] `character_past_line_end_clamps_to_line_end`
  - [ ] `line_past_eof_clamps_to_file_len`
  - [ ] `byte_to_position_inverts_position_to_byte`
  - [ ] `byte_to_position_clamps_offset_inside_multibyte_char`
  - [ ] `byte_to_position_past_eof_clamps`
  - [ ] `range_round_trips`
  - [ ] `property_random_mixed_strings`

**`server/src/code_intel/lsp_provider.rs`**

- mod `tests` (58 tests):
  - [ ] `provider_status_updates_include_provider_language_and_progress`
  - [ ] `repeated_identical_status_is_deduped`
  - [ ] `progress_reports_are_coalesced_but_phase_changes_emit`
  - [ ] `converts_lsp_diagnostic_to_byte_range`
  - [ ] `malformed_diagnostic_is_skipped_not_fabricated`
  - [ ] `file_uri_round_trips_with_space_and_non_ascii`
  - [ ] `diagnostics_uri_decodes_back_to_stored_path`
  - [ ] `resubscribe_preserves_didopen_text_and_still_delivers_diagnostics`
  - [ ] `unsubscribe_didcloses_and_resubscribe_replays_cached_diagnostics`
  - [ ] `subscribe_replays_diagnostics_published_while_file_was_closed`
  - [ ] `deleted_file_closes_document_and_clears_diagnostics`
  - [ ] `publish_diagnostics_updates_provider_error_and_warning_totals`
  - [ ] `unavailable_provider_reprobes_discovery_after_backoff`
  - [ ] `severity_maps_each_lsp_level`
  - [ ] `parse_single_location_and_array_and_link_shapes`
  - [ ] `hover_contents_markup_marked_and_array`
  - [ ] `convert_hover_empty_is_none`
  - [ ] `shutdown_command_terminates_actor_and_lsp_client`
  - [ ] `resubscribe_same_version_new_stream_replays_status_and_model`
  - [ ] `navigate_resolves_definition_to_byte_range_cross_file`
  - [ ] `navigate_returns_multiple_targets`
  - [ ] `hover_returns_markdown_and_byte_range`
  - [ ] `stale_on_demand_requests_emit_error_and_skip_lsp`
  - [ ] `navigate_drops_inflight_result_after_source_version_changes`
  - [ ] `navigate_during_indexing_is_honest_empty`
  - [ ] `client_exit_surfaces_failed_and_schedules_bounded_restart`
  - [ ] `provider_subprocess_crash_emits_observable_restart_fatality`
  - [ ] `restart_redidopens_still_subscribed_files`
  - [ ] `decode_semantic_tokens_to_byte_ranges_multibyte_multiline`
  - [ ] `decode_semantic_tokens_empty_or_unknown_legend_yields_nothing`
  - [ ] `decode_semantic_tokens_saturates_untrusted_end_character`
  - [ ] `reprioritize_moves_visible_occurrences_to_front`
  - [ ] `semantic_tokens_failure_surfaces_error_status_and_model_failed`
  - [ ] `model_failure_clears_pushed_marker_for_retry`
  - [ ] `model_push_streams_partial_then_complete_with_byte_targets`
  - [ ] `model_push_cancelled_before_decode_emits_nothing`
  - [ ] `subscribe_at_new_version_restarts_model_push`
  - [ ] `unsubscribe_cancels_resolution_handle`
  - [ ] `cancellation_stops_definition_frames`
  - [ ] `watcher_version_change_reopens_and_repushes_at_new_version`
  - [ ] `version_change_for_unsubscribed_file_is_ignored`
  - [ ] `references_progress_splits_live_cancel_supersede`
  - [ ] `build_reference_lines_groups_by_line_with_byte_previews`
  - [ ] `build_reference_lines_marks_truncation_at_cap`
  - [ ] `find_references_streams_per_file_results_then_complete`
  - [ ] `find_references_superseded_drops_all_frames`
  - [ ] `find_references_cancel_midflight_sends_lsp_cancel_for_request_id`
  - [ ] `find_references_supersede_midflight_sends_lsp_cancel_for_request_id`
  - [ ] `find_references_cancelled_emits_cancelled_complete`
  - [ ] `find_references_request_failure_completes_with_error`
  - [ ] `provider_find_references_honest_empty_when_not_ready`
  - [ ] `is_large_file_triggers_on_bytes_or_occurrences`
  - [ ] `bounding_range_spans_indices`
  - [ ] `chunk_plan_orders_visible_window_first`
  - [ ] `resource_caps_tighten_under_load_and_limited`
  - [ ] `large_file_streams_byte_range_chunks_visible_first_then_complete`
  - [ ] `limited_mode_still_converges_to_full_file_complete`
  - [ ] `large_file_model_cancelled_mid_stream_emits_no_complete`

**`server/src/code_intel/mod.rs`**

- mod `tests` (5 tests):
  - [ ] `extension_maps_to_each_language`
  - [ ] `each_language_has_a_distinct_config`
  - [ ] `detect_requires_extension_and_matching_marker`
  - [ ] `detect_rejects_extension_without_its_marker`
  - [ ] `project_language_detection_uses_root_markers_without_files`

**`server/src/code_intel/provider.rs`**

- mod `mock` (2 tests):
  - [ ] `supported_file_is_ready_with_model`
  - [ ] `unsupported_file_is_unsupported_without_model`

**`server/src/code_intel/pyright.rs`**

- mod `tests` (8 tests):
  - [ ] `pyright_config_identifies_as_python`
  - [ ] `pyright_config_reads_configured_path`
  - [ ] `configured_pyright_path_is_used_directly`
  - [ ] `invalid_configured_pyright_path_fails_without_install_hint`
  - [ ] `prefers_langserver_over_bare_pyright_binary`
  - [ ] `falls_back_to_bare_pyright_with_stdio`
  - [ ] `absent_emits_install_hint_when_neither_found`
  - [ ] `pyright_emits_diagnostics_for_broken_file`

**`server/src/code_intel/rust_analyzer.rs`**

- mod `tests` (2 tests):
  - [ ] `rust_config_identifies_as_rust_analyzer`
  - [ ] `rust_analyzer_emits_diagnostics_for_broken_file`

**`server/src/code_intel/service.rs`**

- mod `tests` (2 tests):
  - [ ] `retain_roots_shuts_down_retired_services_before_settings_fanout`
  - [ ] `shutdown_all_drains_services_and_sends_shutdown`

**`server/src/config_mcp.rs`**

- mod `tests` (5 tests):
  - [ ] `tool_list_includes_skill_and_mcp_mutations`
  - [ ] `backend_status_derives_configured_acp_agents_from_settings`
  - [ ] `backend_status_response_shape_remains_stable`
  - [ ] `backend_status_handler_seam_passes_configured_agents`
  - [ ] `backend_status_handler_seam_stops_on_settings_failure`

**`server/src/connection.rs`**

- mod `tests` (7 tests):
  - [ ] `stale_audio_is_sequence_admitted_before_lifecycle_drop`
  - [ ] `writer_interleaves_fragments_without_sequence_holes`
  - [ ] `mobile_terminal_blocklist_covers_all_terminal_control_frames`
  - [ ] `only_mobile_connections_have_a_peer_liveness_deadline`
  - [ ] `peer_broken_pipe_is_a_clean_disconnect_but_other_io_errors_survive`
  - [ ] `set_setting_command_errors_have_value_free_typed_targets`
  - [ ] `malformed_set_setting_errors_are_typed_and_other_errors_remain_compatible`

**`server/src/debug_mcp.rs`**

- mod `tests` (15 tests):
  - [ ] `rejects_non_loopback_bind_addr`
  - [ ] `write_dev_config_overrides_frontend_port`
  - [ ] `tauri_dev_command_disables_watch`
  - [ ] `tauri_dev_command_sets_resolved_path`
  - [ ] `startup_diagnostics_include_bounded_output_tail`
  - [ ] `debug_capabilities_fail_closed_for_unsupported_surfaces`
  - [ ] `debug_events_exposes_resume_cursor_without_empty_event`
  - [ ] `dev_instance_environment_isolates_every_mutable_path`
  - [ ] `disposable_hermes_environment_is_attested_and_denies_provider_egress`
  - [ ] `start_input_keeps_hermes_isolation_opt_in`
  - [ ] `toolchain_homes_survive_disposable_home_redirection`
  - [ ] `trunk_cache_survives_while_home_remains_isolated`
  - [ ] `disposable_profile_grammar_matches_hermes_discovery`
  - [ ] `dev_instance_seeds_only_requested_project`
  - [ ] `split_request_target_reads_percent_encoded_repo_root`

**`server/src/hermes_mcp_bridge.rs`**

- mod `tests` (5 tests):
  - [ ] `bridge_aggregates_and_routes_http_tools`
  - [ ] `duplicate_downstream_tool_names_are_rejected`
  - [ ] `unavailable_downstream_does_not_hide_working_tools`
  - [ ] `stalled_downstream_does_not_block_working_tools`
  - [ ] `missing_descriptor_is_an_inert_bridge`

**`server/src/host.rs`**

- mod `tests` (119 tests):
  - [ ] `bootstrap_delivery_claims_only_uncovered_visibility`
  - [ ] `installed_backend_version_selects_exact_backend_setup_value`
  - [ ] `team_member_result_uses_terminal_notify_logical_session`
  - [ ] `default_compaction_prompt_matches_approved_handoff_note`
  - [ ] `hidden_helpers_keep_hermes_scope_with_non_increasing_effort`
  - [ ] `hermes_profile_launch_entries_synthesize_ready_and_unavailable`
  - [ ] `passive_adapter_ingress_is_isolated_per_host_channel`
  - [ ] `backend_native_child_reply_drop_is_a_typed_error_not_a_panic`
  - [ ] `session_registration_precedes_later_spawn_work_for_native_children`
  - [ ] `early_visible_native_child_closes_without_exposing_its_parent`
  - [ ] `bootstrap_records_visible_native_child_but_omits_pending_parent`
  - [ ] `cancelled_spawn_cleans_unpublished_session_registration`
  - [ ] `startup_failure_cleans_unpublished_session_registration`
  - [ ] `simultaneous_startup_failure_and_fanout_publish_one_terminal_agent`
  - [ ] `simultaneous_startup_failure_claim_prevents_unpublished_fanout`
  - [ ] `synchronous_parent_fanout_closes_every_advertised_subscriber`
  - [ ] `fanout_uncovered_bootstrap_is_exactly_once_and_closes`
  - [ ] `bootstrap_first_publication_claims_spawn_success`
  - [ ] `bootstrap_includes_visible_unpublished_normal_spawn_once`
  - [ ] `cancelled_visibility_excludes_even_a_publicly_bound_agent_from_bootstrap`
  - [ ] `session_publication_follows_new_agent_fanout`
  - [ ] `pending_agent_annotation_promotes_only_at_session_publication`
  - [ ] `startup_mcp_servers_attach_config_only_to_help_agent`
  - [ ] `antigravity_session_summary_resumability_requires_native_db`
  - [ ] `a_full_replay_page_limit_is_accepted_when_echoed_back`
  - [ ] `full_replay_reports_no_limit_rather_than_the_session_count`
  - [ ] `session_page_limit_bounds_resolved_limits_not_just_requested_ones`
  - [ ] `session_count_update_applies_only_to_snapshots_containing_session`
  - [ ] `cancelled_fanout_still_authorizes_and_signals_publication`
  - [ ] `summary_from_same_session_agent_waits_for_other_agent_new_agent`
  - [ ] `dropped_fanout_batch_releases_subscriber_count_hold`
  - [ ] `spawn_host_with_mock_backend_does_not_require_existing_tokio_runtime`
  - [ ] `activity_summary_timeout_error_clears_in_flight_and_emits_error`
  - [ ] `activity_summary_generation_timeout_returns_error`
  - [ ] `agent_name_generation_timeout_returns_error`
  - [ ] `queued_activity_summary_waits_for_permit_before_pending_state`
  - [ ] `delete_project_removes_code_intel_router`
  - [ ] `restarted_project_stream_recreates_code_intel_router_handle`
  - [ ] `dynamic_session_schema_unavailable_rejects_explicit_settings`
  - [ ] `dynamic_session_schema_unavailable_rejects_stored_settings`
  - [ ] `stored_session_settings_invalid_for_schema_are_rejected`
  - [ ] `worktree_path_sanitizes_branch_characters`
  - [ ] `workbench_remove_reports_internal_when_parent_record_is_missing`
  - [ ] `task_token_usage_rollup_preserves_partial_root_self_usage`
  - [ ] `task_token_usage_rollup_mixes_registered_and_native_children_once`
  - [ ] `response_end_fans_out_targeted_assistant_response_count`
  - [ ] `explicit_session_list_delivery_subscribes_each_host_stream`
  - [ ] `explicit_session_list_precedes_pending_count_drain`
  - [ ] `empty_session_snapshot_delivers_each_count_and_unsubscribe_discards`
  - [ ] `lazy_host_registration_defers_agent_bootstrap_until_load`
  - [ ] `task_token_usage_keeps_unresponsive_live_agent_unavailable`
  - [ ] `unchanged_task_token_usage_is_not_resent_after_bootstrap`
  - [ ] `bootstrapping_new_agent_fanout_is_deferred_until_after_bootstrap`
  - [ ] `forced_backend_config_snapshot_fanout_reemits_unchanged_native_settings`
  - [ ] `backend_setup_refresh_order_is_authoritative`
  - [ ] `forced_session_schema_fanout_reemits_unchanged_snapshot`
  - [ ] `host_reload_reprobes_ready_dynamic_session_schemas`
  - [ ] `native_agent_compaction_route_does_not_block_host_commands`
  - [ ] `native_agent_compaction_route_orders_later_input_after_terminal`
  - [ ] `legacy_agent_compaction_route_preserves_transcript_and_replaces_session`
  - [ ] `agent_compaction_rotates_user_agent`
  - [ ] `agent_compaction_completed_validates_before_old_agent_closed_on_instance_stream`
  - [ ] `agent_compaction_rejects_busy_agent`
  - [ ] `agent_compaction_summary_failure_leaves_old_agent`
  - [ ] `agent_compaction_replacement_failure_leaves_old_agent`
  - [ ] `agent_compaction_rotates_team_member_session`
  - [ ] `native_team_compaction_defers_busy_member_until_idle`
  - [ ] `native_team_compaction_preserves_live_idle_member_bindings`
  - [ ] `team_first_message_records_report_session_id`
  - [ ] `team_member_spawn_uses_union_of_project_roots`
  - [ ] `team_terminal_agent_unbinds_and_resumes_next_message`
  - [ ] `team_subsequent_unbound_message_resumes_session`
  - [ ] `team_message_member_rejects_report_caller`
  - [ ] `team_resume_failure_marks_binding_failed`
  - [ ] `team_delete_hard_removes_team_and_members`
  - [ ] `concurrent_first_team_messages_spawn_at_most_one_agent`
  - [ ] `team_delete_rejects_live_bound_member`
  - [ ] `team_references_block_custom_agent_but_project_delete_unassigns_members`
  - [ ] `create_member_and_delete_custom_agent_serialize`
  - [ ] `create_member_and_delete_project_serialize`
  - [ ] `ai_reviewer_backend_resolution_uses_host_defaults`
  - [ ] `ai_reviewer_non_claude_reaches_read_only_spawn_preconditions`
  - [ ] `supervisor_done_deadline_uses_live_delay_and_original_idle_since`
  - [ ] `supervisor_stall_deadline_tracks_progress_not_turn_start`
  - [ ] `supervisor_stall_recheck_floor_defers_then_clears_on_settings_edit`
  - [ ] `supervisor_restore_gate_defers_until_the_setting_is_enabled`
  - [ ] `supervisor_failed_gate_is_suppressed_for_only_its_settings_epoch`
  - [ ] `supervisor_fresh_idle_observation_starts_a_new_interval`
  - [ ] `supervisor_kick_is_active_before_backend_typing_starts`
  - [ ] `settings_commit_during_verdict_await_rearms_without_kick`
  - [ ] `deadline_launch_reads_live_epoch_without_rearm_churn`
  - [ ] `disable_then_enable_observes_idle_agent_with_fresh_interval`
  - [ ] `activity_cancels_pending_retry_and_resets_generation`
  - [ ] `failed_verdict_retries_then_continue_delivers_one_kick`
  - [ ] `failure_exhaustion_appends_once_and_only_then_becomes_dormant`
  - [ ] `non_retryable_supervisor_backend_failure_stops_without_warning`
  - [ ] `settings_change_at_warning_gate_rejects_stale_append_and_preserves_backoff`
  - [ ] `failure_backed_live_cap_reduction_warns_but_settings_only_does_not`
  - [ ] `aborted_verdict_task_releases_permit_and_reports_completion`
  - [ ] `production_retry_scheduler_starts_exact_bounded_calls_at_due_deadlines`
  - [ ] `production_scheduler_occupancy_survives_activity_and_disable_phase_resets`
  - [ ] `actor_verdict_start_rejects_activity_ordered_before_authorization`
  - [ ] `production_scheduler_actor_gate_rejects_stale_settings`
  - [ ] `production_scheduler_serializes_due_agents_through_one_task_owner`
  - [ ] `actor_gate_rejects_each_stale_supervisor_setting_kind`
  - [ ] `actor_gate_linearizes_activity_before_conditional_compaction`
  - [ ] `actor_gate_accepts_compaction_that_linearizes_first`
  - [ ] `supervisor_retry_limit_changes_preserve_or_exhaust_pending_retry`
  - [ ] `supervisor_retry_backoff_and_caps_are_exact_and_finite`
  - [ ] `supervisor_retry_deadlines_serialize_and_each_agent_stops_at_default_cap`
  - [ ] `backend_compaction_mutation_and_method_conversions_are_exhaustive`
  - [ ] `capability_conversion_includes_policy_and_transcript_authority`
  - [ ] `threshold_zero_remains_blocked_until_a_real_user_message`
  - [ ] `finished_compaction_remains_dormant_without_a_deadline`
  - [ ] `post_compaction_guard_holds_without_mark_compacted_at_both_sites`
  - [ ] `legacy_lineage_guard_remains_compatible_at_both_sites`
  - [ ] `compaction_barrier_deadline_extends_long_stall_timeout`
  - [ ] `configured_acp_profiles_always_include_the_builtin_kiro_agent`
  - [ ] `acp_schemas_are_per_agent_and_do_not_leak_across_profiles`

**`server/src/mobile_access.rs`**

- mod `tests` (27 tests):
  - [ ] `mobile_pairings_lease_rejects_second_holder`
  - [ ] `disabled_snapshot_is_idle`
  - [ ] `startup_discards_persisted_dev_active_pairing`
  - [ ] `startup_discards_persisted_dev_active_pairing_when_disabled`
  - [ ] `successful_device_reconnect_restores_online_broker_status`
  - [ ] `startup_marks_public_broker_pairing_repair_required`
  - [ ] `enabling_without_pairing_requires_managed_repair`
  - [ ] `repair_required_device_accept_failure_stops_retry_task`
  - [ ] `service_unavailable_error_code_is_parsed_as_typed_error`
  - [ ] `host_handoff_expired_error_maps_to_pairing_expired`
  - [ ] `managed_service_dto_debug_redacts_broker_secrets`
  - [ ] `managed_service_connect_dto_allows_omitted_unused_fields`
  - [ ] `plaintext_public_broker_url_reports_invalid_config`
  - [ ] `ipv6_loopback_broker_url_reports_online`
  - [ ] `public_ipv6_broker_url_reports_invalid_config`
  - [ ] `pairing_offer_contains_configured_mqtt_qr`
  - [ ] `managed_pairing_start_calls_service_and_surfaces_host_built_qr`
  - [ ] `managed_handoff_is_durable_before_ack_and_broker_retry`
  - [ ] `crash_before_ack_resumes_ack_and_broker_from_durable_state`
  - [ ] `expired_handoff_after_crash_keeps_pairing_and_allows_new_offer`
  - [ ] `startup_resumes_managed_offer_poll_without_durable_record`
  - [ ] `handoff_ack_retry_backoff_is_exponential_and_capped`
  - [ ] `repeated_ack_failure_does_not_emit_duplicate_failed_state`
  - [ ] `managed_poll_rejects_an_offer_identity_mismatch`
  - [ ] `redeemed_managed_poll_requires_typed_handoff_state`
  - [ ] `redeemed_managed_poll_accepts_expired_handoff_without_secrets`
  - [ ] `pairing_offer_is_sent_only_to_requesting_stream`

**`server/src/paths.rs`**

- mod `tests` (4 tests):
  - [ ] `uses_home_when_available`
  - [ ] `falls_back_to_userprofile`
  - [ ] `falls_back_to_homedrive_and_homepath`
  - [ ] `ignores_empty_values`

**`server/src/process_env.rs`**

- mod `tests` (2 tests):
  - [ ] `extract_probe_value_finds_payload_amidst_noise`
  - [ ] `extract_probe_value_returns_none_without_sentinels`

**`server/src/project_stream.rs`**

- mod `tests` (53 tests):
  - [ ] `build_git_status_uses_read_only_git_access`
  - [ ] `build_git_status_retains_porcelain_v2_unmerged_records`
  - [ ] `code_intel_overview_starts_idle_for_every_root`
  - [ ] `overview_summary_sums_provider_diagnostic_counts`
  - [ ] `code_intel_overview_replaces_provider_status_and_summarizes`
  - [ ] `code_intel_overview_root_sync_prunes_removed_roots`
  - [ ] `read_diff_uses_read_only_git_access`
  - [ ] `read_diff_returns_combined_sections_as_unmerged_files`
  - [ ] `list_untracked_paths_returns_correct_files`
  - [ ] `read_diff_reports_paths_relative_to_subdirectory_root`
  - [ ] `build_untracked_diff_file_all_added_lines`
  - [ ] `build_untracked_diff_file_missing_file_returns_err`
  - [ ] `build_untracked_diff_file_binary_returns_binary_file_without_hunks`
  - [ ] `read_diff_includes_untracked_binary_file_without_hunks`
  - [ ] `parse_git_diff_emits_prefix_free_text_and_typed_line_numbers`
  - [ ] `parse_git_diff_keeps_combined_sections_typed_and_separate`
  - [ ] `parse_hunk_header_identifies_stray_combined_headers`
  - [ ] `parse_git_diff_marks_tracked_binary_sections`
  - [ ] `parse_git_diff_keeps_mode_only_sections_as_no_hunk_files`
  - [ ] `read_only_git_commands_disable_optional_locks`
  - [ ] `git_commands_force_c_locale_for_parseable_output`
  - [ ] `mutating_git_commands_do_not_disable_locks`
  - [ ] `search_finds_literal_matches_across_files`
  - [ ] `search_respects_gitignore_by_default`
  - [ ] `search_include_ignored_overrides_gitignore`
  - [ ] `search_skips_git_directory`
  - [ ] `search_skips_binary_files`
  - [ ] `search_literal_does_not_treat_query_as_regex`
  - [ ] `search_regex_mode_matches_pattern`
  - [ ] `search_case_sensitive_flag`
  - [ ] `search_whole_word_flag`
  - [ ] `search_invalid_regex_errors`
  - [ ] `search_empty_query_errors`
  - [ ] `search_ranges_are_byte_offsets_for_multibyte_lines`
  - [ ] `search_enforces_max_results_cap`
  - [ ] `search_never_exceeds_global_match_cap`
  - [ ] `search_path_prefix_scopes_to_folder`
  - [ ] `search_multi_root_attributes_paths_to_correct_root`
  - [ ] `search_rejects_root_outside_project`
  - [ ] `search_cancellation_stops_walk`
  - [ ] `watched_change_bumps_version_exactly_once_and_is_monotonic`
  - [ ] `reads_peek_the_counter_only_the_watcher_bumps_it`
  - [ ] `git_internal_changes_do_not_bump_or_notify`
  - [ ] `access_and_metadata_watch_events_do_not_bump_versions`
  - [ ] `git_index_metadata_event_refreshes_git_without_code_intel_bump`
  - [ ] `git_index_access_event_does_not_refresh_or_bump_code_intel`
  - [ ] `source_metadata_event_does_not_refresh_or_bump_code_intel`
  - [ ] `content_watch_events_bump_versions_and_coalesce_duplicate_paths`
  - [ ] `canonicalized_watcher_paths_map_back_to_symlinked_roots`
  - [ ] `watcher_root_resolver_syncs_only_on_root_change`
  - [ ] `separate_content_events_coalesce_to_latest_pending_version`
  - [ ] `register_listener_and_current_version_is_one_serialized_step`
  - [ ] `notify_listeners_delivers_changes_and_prunes_closed`

**`server/src/review/actor.rs`**

- mod `tests` (8 tests):
  - [ ] `validate_location_accepts_correct_line_sides`
  - [ ] `validate_location_rejects_wrong_side_and_out_of_range`
  - [ ] `validate_location_accepts_file_anchor_for_binary_file`
  - [ ] `diff_is_clean_treats_binary_file_as_dirty`
  - [ ] `diff_is_clean_treats_metadata_only_file_as_dirty`
  - [ ] `diff_is_clean_accepts_only_empty_file_lists`
  - [ ] `refresh_anchor_statuses_marks_comments_stale_without_reanchoring`
  - [ ] `ai_suggestion_state_is_pending_before_insert`

**`server/src/review/bundle.rs`**

- mod `tests` (1 tests):
  - [ ] `bundle_renders_deterministic_markdown`

**`server/src/review/mod.rs`**

- mod `tests` (4 tests):
  - [ ] `running_ai_reviewer_resets_to_idle_on_rehydrate`
  - [ ] `non_running_ai_reviewer_does_not_need_persist_on_rehydrate`
  - [ ] `summary_counts_file_comment_semantics`
  - [ ] `lightweight_review_subscriber_payloads_redact_diffs`

**`server/src/review/reviewer.rs`**

- mod `tests` (2 tests):
  - [ ] `tool_args_convert_to_pending_suggestion`
  - [ ] `reviewer_prompt_uses_diff_roots_for_tool_locations`

**`server/src/store/agent_teams.rs`**

- mod `tests` (13 tests):
  - [ ] `member_create_persists_and_load_round_trips`
  - [ ] `v3_migration_assigns_legacy_default_backend`
  - [ ] `v4_migration_defaults_member_profile_to_none`
  - [ ] `v3_migration_rejects_missing_legacy_default_backend`
  - [ ] `migrates_gemini_members_and_purged_session_refs`
  - [ ] `migrates_kiro_members_to_acp_and_keeps_their_sessions`
  - [ ] `invalid_migrated_gemini_members_are_not_written`
  - [ ] `member_create_allows_no_custom_agent_with_explicit_backend`
  - [ ] `deleting_active_manager_is_rejected`
  - [ ] `set_member_session_id_rejects_duplicate_session_owner`
  - [ ] `update_member_validates_project_references`
  - [ ] `create_member_allows_empty_project_ids`
  - [ ] `remove_project_clears_deleted_member_sessions`

**`server/src/store/agents_view_preferences.rs`**

- mod `tests` (7 tests):
  - [ ] `corrupt_load_uses_defaults_and_reports_error`
  - [ ] `valid_mutation_overwrites_corrupt_file_and_clears_error`
  - [ ] `duplicate_manual_order_is_rejected`
  - [ ] `remove_project_prunes_current_and_saved_filters`
  - [ ] `legacy_store_migrates_to_empty_smart_views`
  - [ ] `legacy_kiro_backend_filter_migrates_instead_of_corrupting_the_store`
  - [ ] `sidebar_preferences_persist_and_v4_migrates_to_default`

**`server/src/store/custom_agents.rs`**

- mod `tests` (7 tests):
  - [ ] `fresh_store_seeds_exactly_default_orchestrator_help`
  - [ ] `load_upgrades_unedited_legacy_team_lead_to_orchestrator`
  - [ ] `load_upgrades_unedited_v2_orchestrator_to_backend_domain`
  - [ ] `load_upgrades_unedited_v3_orchestrator_to_backend_domain`
  - [ ] `load_preserves_edited_team_lead`
  - [ ] `deprecated_builtins_do_not_reseed`
  - [ ] `superseded_detection_matches_published_versions_only`

**`server/src/store/legacy_backend_kind.rs`**

- mod `tests` (3 tests):
  - [ ] `renames_scalar_and_list_backend_kinds_at_any_depth`
  - [ ] `leaves_user_authored_names_alone`
  - [ ] `reports_no_change_when_already_migrated`

**`server/src/store/mobile_pairings.rs`**

- mod `tests` (8 tests):
  - [ ] `pairings_save_round_trips_active_pairing_and_devices`
  - [ ] `protected_active_managed_pairing_round_trips`
  - [ ] `durable_managed_pairing_credentials_round_trip`
  - [ ] `managed_pairing_record_replay_is_idempotent`
  - [ ] `managed_pairing_record_replay_rejects_conflicting_secret`
  - [ ] `unknown_store_version_loads_as_repair_required`
  - [ ] `incomplete_managed_metadata_starts_empty_instead_of_failing_load`
  - [ ] `debug_redacts_mobile_pairing_secrets`

**`server/src/store/project.rs`**

- mod `tests` (10 tests):
  - [ ] `migrates_v1_records_to_version_2_standalone_records`
  - [ ] `validation_rejects_invalid_standalone_roots`
  - [ ] `validation_rejects_workbench_with_missing_parent`
  - [ ] `validation_rejects_workbench_parent_root_not_in_parent`
  - [ ] `load_heals_duplicate_standalone_roots_instead_of_failing`
  - [ ] `load_keeps_shared_roots_a_workbench_depends_on`
  - [ ] `load_tolerates_legacy_standalone_project_with_no_roots`
  - [ ] `load_quarantines_workbench_with_missing_parent`
  - [ ] `validation_rejects_duplicate_actual_roots_across_records`
  - [ ] `replay_order_lists_standalone_projects_before_grouped_workbenches`

**`server/src/store/review.rs`**

- mod `tests` (4 tests):
  - [ ] `review_store_round_trips_records`
  - [ ] `review_store_compacts_legacy_diff_snapshots_on_load`
  - [ ] `review_store_migrates_legacy_agent_origin`
  - [ ] `review_store_migrates_legacy_project_origin`

**`server/src/store/session.rs`**

- mod `tests` (9 tests):
  - [ ] `session_store_purges_legacy_gemini_records_on_load`
  - [ ] `session_store_loads_legacy_records_without_compaction_fields`
  - [ ] `session_store_marks_legacy_synthetic_antigravity_sessions_non_resumable`
  - [ ] `session_summaries_mark_native_antigravity_missing_db_non_resumable`
  - [ ] `antigravity_record_resumability_allows_transient_missing_db_to_recover`
  - [ ] `antigravity_record_resumability_preserves_permanent_false_records`
  - [ ] `session_store_round_trips_task_list`
  - [ ] `delete_for_project_removes_sessions_and_task_lists_only_for_target`
  - [ ] `restart_never_redispatches_an_ambiguous_native_operation`

**`server/src/store/settings.rs`**

- mod `tests` (21 tests):
  - [ ] `seeds_installed_backends_on_fresh_install_with_preferred_default`
  - [ ] `hermes_disabled_providers_are_per_profile_and_clear_to_absent`
  - [ ] `seeding_is_noop_once_a_settings_file_exists`
  - [ ] `seeding_is_noop_when_nothing_is_installed`
  - [ ] `mobile_broker_url_write_accepts_only_loopback_dev_brokers`
  - [ ] `legacy_public_mobile_broker_url_still_loads_for_repair_state`
  - [ ] `old_store_files_without_tier_fields_load_with_tiers_off`
  - [ ] `old_store_files_default_background_agent_features_safely`
  - [ ] `background_agent_feature_settings_apply_independently`
  - [ ] `unknown_backend_in_enabled_backends_is_skipped`
  - [ ] `unknown_backend_tier_config_key_is_skipped`
  - [ ] `unknown_default_backend_falls_back_to_none`
  - [ ] `fully_known_settings_file_round_trips_unchanged`
  - [ ] `validates_and_persists_agent_control_max_depth`
  - [ ] `migrates_gemini_settings_to_antigravity`
  - [ ] `migrates_kiro_settings_to_acp`
  - [ ] `kiro_migration_does_not_clobber_an_existing_acp_config`
  - [ ] `stock_acp_launch_profile_requires_a_command`
  - [ ] `agent_spec_on_a_non_acp_profile_is_rejected`
  - [ ] `enabling_complexity_tiers_seeds_builtin_configs_and_round_trips`
  - [ ] `voice_settings_validate_exact_model_without_fallback`

**`server/src/store/skills.rs`**

- mod `tests` (12 tests):
  - [ ] `list_accepts_skill_without_metadata`
  - [ ] `list_skips_malformed_metadata_without_failing`
  - [ ] `skill_paths_resolve_inside_the_store_root`
  - [ ] `skill_paths_report_a_missing_directory_instead_of_guessing`
  - [ ] `skill_paths_reject_a_skill_md_symlink_that_escapes_the_skill_dir`
  - [ ] `skill_paths_accept_a_skill_md_symlink_inside_the_skill_dir`
  - [ ] `skill_paths_reject_a_skill_md_that_is_not_a_regular_file`
  - [ ] `validate_skill_keeps_accepting_names_existing_stores_already_use`
  - [ ] `invalid_names_cannot_mutate_outside_the_store_root`
  - [ ] `canonical_within_rejects_a_symlink_that_escapes_the_root`
  - [ ] `duplicate_skill_ids_resolve_to_the_first_directory_by_name`
  - [ ] `load_rebuilds_invalid_index_from_disk`

**`server/src/store/transcript.rs`**

- mod `tests` (4 tests):
  - [ ] `append_load_and_window_preserve_marker_order`
  - [ ] `provider_import_is_idempotent`
  - [ ] `authority_is_explicit`
  - [ ] `safe_id_never_creates_subdirectories`

**`server/src/stream.rs`**

- mod `tests` (13 tests):
  - [ ] `control_precedes_chat_bulk_and_audio_is_eight_packets`
  - [ ] `dependencies_preserve_bootstrap_while_control_preempts_unstarted_bulk`
  - [ ] `new_agent_and_bootstrap_gate_chat_without_blocking_other_control`
  - [ ] `agent_close_shares_chat_fifo_without_blocking_control`
  - [ ] `compacted_session_precedes_completed_notify_while_control_progresses`
  - [ ] `session_schema_precedes_catalog_while_control_progresses`
  - [ ] `cleared_queue_precedes_fatal_error_while_control_progresses`
  - [ ] `observer_receiver_completion_releases_bootstrap_chat`
  - [ ] `project_snapshot_and_status_share_fifo_bulk_lane`
  - [ ] `output_family_classification_contract_is_explicit`
  - [ ] `every_frame_is_classified_and_control_overflow_is_fatal`
  - [ ] `malformed_audio_is_rejected_and_session_purge_reports_exact_drops`
  - [ ] `output_channel_uses_scheduler_order_and_closes_with_last_stream`

**`server/src/team_registry.rs`**

- mod `tests` (6 tests):
  - [ ] `plan_user_activation_fresh_member_is_new`
  - [ ] `plan_user_activation_no_reserve_does_not_block_followup`
  - [ ] `plan_user_activation_with_session_is_resume`
  - [ ] `plan_user_activation_with_binding_is_reuse`
  - [ ] `plan_user_activation_rejects_missing_member`
  - [ ] `plan_user_activation_rejects_deleted_member`

**`server/src/terminal_stream.rs`**

- mod `tests` (6 tests):
  - [ ] `complete_csi_and_prompt_text_emit_immediately`
  - [ ] `split_ansi_is_not_aligned_by_the_server`
  - [ ] `split_multibyte_utf8_reassembles_exactly`
  - [ ] `complete_ascii_and_utf8_emit_immediately`
  - [ ] `eof_flushes_pending_partial_utf8_lossily`
  - [ ] `trusted_command_uses_exact_program_and_arguments`

**`server/src/voice.rs`**

- mod `tests` (4 tests):
  - [ ] `long_agent_turn_resets_inactivity_and_still_completes`
  - [ ] `opus_48khz_encode_decodes_directly_at_16khz`
  - [ ] `tool_result_is_utf8_bounded_and_marks_truncation`
  - [ ] `observer_guard_closes_production_queue_on_every_exit`

**`server/src/voice_aws.rs`**

- mod `tests` (9 tests):
  - [ ] `opening_sends_closed_system_content_before_the_microphone`
  - [ ] `expired_sso_is_typed_without_leaking_provider_text`
  - [ ] `prompt_lifetime_carries_exactly_one_system_block`
  - [ ] `mid_session_error_detail_extracts_the_service_message`
  - [ ] `provider_events_are_typed_and_interrupted_completion_audio_stays_stale`
  - [ ] `interrupt_is_local_generation_control_and_aws_barge_in_remains_continuous_audio`
  - [ ] `opening_uses_configured_endpointing_sensitivity`
  - [ ] `interrupt_barrier_precedes_stalled_input_stream_delivery`
  - [ ] `real_nova_sonic_startup_uses_exact_configured_model`

**`server/src/workflows/registry.rs`**

- mod `tests` (4 tests):
  - [ ] `select_control_requires_options`
  - [ ] `select_default_must_match_options_and_valid_default_parses`
  - [ ] `legacy_input_type_alias_migrates_known_and_warns_on_unknown`
  - [ ] `project_shadowing_is_scoped_and_bad_files_emit_diagnostics`

**`server/src/workflows/store.rs`**

- mod `tests` (4 tests):
  - [ ] `legacy_kiro_coordinator_migrates_instead_of_failing_the_whole_store`
  - [ ] `load_marks_running_runs_failed`
  - [ ] `delete_for_project_removes_only_matching_runs_and_persists`
  - [ ] `workflow_run_store_path_honors_override`

### tyde-server

**`tyde-server/src/main.rs`**

- mod `tests` (3 tests):
  - [ ] `socket_contention_exits_before_host_start`
  - [ ] `parses_host_modes`
  - [ ] `parses_version`

## Deleted standalone test files

**`protocol/tests/review_protocol.rs`**
- [ ] `review_ids_round_trip`
- [ ] `review_data_model_round_trips`
- [ ] `review_payload_structs_round_trip`
- [ ] `review_action_tagged_union_variants_round_trip`
- [ ] `review_comment_anchor_status_defaults_for_legacy_json`
- [ ] `project_git_diff_file_binary_flag_defaults_for_legacy_json`
- [ ] `review_summary_scope_defaults_for_legacy_json`
- [ ] `review_file_comment_count_round_trips_and_totals`
- [ ] `review_start_ai_backend_kind_is_optional`
- [ ] `review_subscribe_payload_defaults_to_include_diffs`
- [ ] `project_git_diff_file_binary_flag_round_trips`
- [ ] `review_event_tagged_union_variants_round_trip`
- [ ] `review_error_codes_and_contexts_round_trip`
- [ ] `message_origin_round_trips_and_defaults_to_none`
- [ ] `new_frame_kinds_and_project_diff_scope_use_snake_case`
- [ ] `bootstrap_payloads_round_trip`
**`protocol/tests/tool_request_type.rs`**
- [ ] `ask_user_question_tool_request_round_trips`
- [ ] `exit_plan_mode_tool_request_round_trips`
**`mqtt-transport/tests/topic_roundtrip.rs`**
- [ ] `public_topic_round_trip`
**`tests/tests/webrtc_build_support.rs`**

## Test-only plumbing removed (pass 2)

Non-mod `#[cfg(test)]` items (test gates, hooks, timeout twins, diagnostic
fields) deleted, `#[cfg(not(test))]` twins un-gated to unconditional
production code, and `any(test, target_arch = "wasm32")`-style cfgs narrowed
to wasm-only. These were unit-test scaffolding, not coverage, but they are
listed because some encoded timing/ordering assumptions the e2e suite may
need test-support equivalents for.

**`frontend/src/components/center_zone.rs`**
- DELETE: 4 lines: pub(crate) fn current_announcement() -> String {
- DELETE: 4 lines: pub(crate) fn current_alert() -> String {

**`frontend/src/components/command_palette.rs`**
- DELETE: 9 lines: const fn cmd_shift(key: &'static str, shifted_key: &'static str) -> Se

**`frontend/src/highlight_worker.rs`**
- DELETE: 4 lines: fn contains_active(&self, task_id: u64) -> bool {

**`frontend/src/state.rs`**
- KEEP: wasm composer-draft counter plumbing
- KEEP: wasm composer-draft counter plumbing
- KEEP: wasm composer-draft counter plumbing
- DELETE: 2 lines: pub const DUPLICATE_FILE_TABS_DISABLED_REASON: &str = CENTER_TABS_DISA
- DELETE: 2 lines: pub const DUPLICATE_FILE_SOURCE_MISSING_REASON: &str = TAB_SOURCE_MISS
- DELETE: 2 lines: pub const DUPLICATE_FILE_NOT_A_FILE_REASON: &str = "Only files can be
- DELETE: 2 lines: pub const DUPLICATE_FILE_NOT_LOADED_REASON: &str = "Wait for the file
- DELETE: 3 lines: pub const OPEN_TO_SIDE_CROSS_PROJECT_REASON: &str =
- DELETE: 2 lines: pub const OPEN_TO_SIDE_NOTHING_WOULD_REMAIN_REASON: &str = "Nothing wo
- DELETE: 3 lines: pub const AGENT_OPEN_TO_SIDE_CROSS_PROJECT_REASON: &str =
- DELETE: 7 lines: pub fn is_enabled(self) -> bool {
- DELETE: 10 lines: pub fn disabled_reason(self) -> Option<&'static str> {
- DELETE: 10 lines: pub fn disabled_reason(self) -> Option<&'static str> {
- DELETE: 7 lines: pub enum MoveTabRefusal {
- DELETE: 10 lines: impl MoveTabRefusal {
- DELETE: 15 lines: impl TryFrom<MoveTabResult> for MoveTabRefusal {
- DELETE: 21 lines: pub enum AgentOpenToSideResult {
- DELETE: 12 lines: impl AgentOpenToSideResult {
- DELETE: 21 lines: pub enum DiffOpenToSideResult {
- DELETE: 12 lines: impl DiffOpenToSideResult {
- DELETE: 4 lines: pub fn tabs(&self) -> &[Tab] {
- DELETE: 7 lines: pub fn occurrences(&self, content: &TabContent) -> Vec<(PaneId, TabId)
- DELETE: 15 lines: pub fn close_others(&mut self, id: TabId) {
- DELETE: 18 lines: pub fn close_to_right(&mut self, id: TabId) {
- DELETE: 9 lines: pub fn close_all(&mut self) {
- DELETE: 47 lines: fn agent_open_to_side_block_for(
- DELETE: 44 lines: fn diff_open_to_side_block_for(
- DELETE: 10 lines: fn tab_content(&self) -> TabContent {
- DELETE: 8 lines: pub fn defaults_for(project: Option<&ActiveProjectRef>) -> Self {
- DELETE: 11 lines: pub fn duplicate_file_eligibility_at(
- DELETE: 19 lines: pub fn duplicate_file_eligibility_in(
- DELETE: 11 lines: pub fn duplicate_file_at_result(
- DELETE: 18 lines: pub fn agent_open_to_side_eligibility(
- DELETE: 72 lines: pub fn open_agent_chat_to_side(
- DELETE: 8 lines: pub fn diff_open_to_side_eligibility(&self, key: &DiffKey) -> Option<D
- DELETE: 66 lines: pub fn open_diff_to_side(&self, key: DiffKey, label: String) -> DiffOp
- DELETE: 6 lines: pub fn forget_tab_lru(&self, id: TabId) {
- DELETE: 9 lines: pub fn prune_tab_lru(&self) {
- DELETE: 8 lines: pub fn set_active_tab_in_pane(&self, pane: PaneId, id: TabId) -> bool

**`frontend/src/voice.rs`**
- DELETE: 2 lines: reason: &'static str,
- DELETE: 2 lines: reason,

**`frontend/tauri-shell/src/voice_media.rs`**
- DELETE: 9 lines: ActivateTest {
- DELETE: 6 lines: BlockTest {
- DELETE: 16 lines: fn activate_test(
- DELETE: 22 lines: pub(super) fn activate_test_with_sink(
- DELETE: 5 lines: struct TestSession {
- DELETE: 6 lines: pub(super) struct TestSinkObservation {
- DELETE: 2 lines: Test(TestSession),
- DELETE: 12 lines: impl Drop for LiveSession {
- DELETE: 4 lines: Self::Test(session) => {
- DELETE: 9 lines: LiveSession::Test(session) => {
- DELETE: 2 lines: LiveSession::Test(_) => Ok(()),
- DELETE: 21 lines: ControlCommand::ActivateTest {
- DELETE: 12 lines: ControlCommand::BlockTest {

**`mobile-frontend/src/app.rs`**
- REWRITE: any(test, target_arch = "wasm32") -> target_arch = "wasm32"
- REWRITE: any(test, target_arch = "wasm32") -> target_arch = "wasm32"
- REWRITE: any(test, target_arch = "wasm32") -> target_arch = "wasm32"

**`mobile-frontend/src/state.rs`**
- DELETE: 8 lines: pub(crate) fn terminal_context_compaction_operation_count(

**`mqtt-transport/src/client.rs`**
- DELETE: 2 lines: subscribe_barrier: Option<Arc<Barrier>>,
- DELETE: 2 lines: accepted_publish_count: Option<Arc<AtomicUsize>>,
- DELETE: 2 lines: connection_gate: Option<Arc<tokio::sync::Mutex<()>>>,
- DELETE: 2 lines: diagnostic: Option<TestConnectionDiagnostic>,
- DELETE: 7 lines: struct TestConnectionDiagnostic {
- DELETE: 18 lines: impl TestConnectionDiagnostic {
- DELETE: 5 lines: struct TestConnectionGatedLink {
- DELETE: 13 lines: impl TestConnectionGatedLink {
- DELETE: 24 lines: impl MqttLink for TestConnectionGatedLink {
- DELETE: 22 lines: pub(crate) async fn connect_with_test_overrides(
- DELETE: 22 lines: async fn connect_ephemeral_with_test_diagnostic(
- DELETE: 17 lines: let link = {
- DELETE: 2 lines: let link = TestConnectionGatedLink::new(link, overrides.connection_gat
- DELETE: 2 lines: subscribe_barrier: overrides.subscribe_barrier,
- DELETE: 9 lines: if let Some(diagnostic) = overrides.diagnostic.as_ref() {
- DELETE: 9 lines: if let Some(diagnostic) = overrides.diagnostic.as_ref() {
- DELETE: 10 lines: if let Some(diagnostic) = overrides.diagnostic.as_ref() {
- DELETE: 8 lines: let data_overrides = {
- UNGATE: let data_overrides = overrides;
- DELETE: 4 lines: if let Err(error) = &connected {
- DELETE: 15 lines: {
- DELETE: 8 lines: let data_overrides = ConnectOverrides {
- DELETE: 15 lines: {
- DELETE: 2 lines: let mut link = TestConnectionGatedLink::new(link, overrides.connection

**`mqtt-transport/src/config.rs`**
- REWRITE: any(target_arch = "wasm32", test) -> target_arch = "wasm32"
- REWRITE: any(target_arch = "wasm32", test) -> target_arch = "wasm32"

**`mqtt-transport/src/lib.rs`**
- REWRITE: any(target_arch = "wasm32", test) -> target_arch = "wasm32"

**`mqtt-transport/src/link_native.rs`**
- DELETE: 2 lines: accepted_publish_count: Option<Arc<AtomicUsize>>,
- DELETE: 2 lines: diagnostic: Option<TestConnectionDiagnostic>,
- DELETE: 10 lines: pub(crate) struct TestConnectionDiagnosticContext {
- DELETE: 5 lines: struct TestConnectionDiagnostic {
- DELETE: 2 lines: accepted_publish_count: None,
- DELETE: 2 lines: diagnostic: None,
- DELETE: 7 lines: pub(crate) fn set_accepted_publish_count_for_test(
- DELETE: 10 lines: pub(crate) fn set_test_connection_diagnostic(
- DELETE: 19 lines: Event::Incoming(Packet::ConnAck(connack)) => {
- UNGATE: Event::Incoming(Packet::ConnAck(_)) => Ok(LinkEvent::Other),
- DELETE: 6 lines: if result.is_ok()
- DELETE: 13 lines: if let Some(diagnostic) = self.diagnostic.as_ref() {
- DELETE: 14 lines: pub(crate) fn mqtt_options(

**`mqtt-transport/src/protocol_driver.rs`**
- UNGATE: const PUBLISH_ACK_TIMEOUT: Duration = Duration::from_secs(30);
- DELETE: 2 lines: const PUBLISH_ACK_TIMEOUT: Duration = Duration::from_secs(1);
- UNGATE: const CREDIT_BLOCK_TIMEOUT: Duration = Duration::from_secs(10);
- DELETE: 2 lines: const CREDIT_BLOCK_TIMEOUT: Duration = Duration::from_millis(100);
- DELETE: 2 lines: pub(crate) subscribe_barrier: Option<Arc<Barrier>>,
- DELETE: 4 lines: if let Some(barrier) = self.configured_subscribe_barrier() {
- DELETE: 4 lines: fn configured_subscribe_barrier(&self) -> Option<Arc<Barrier>> {
- DELETE: 4 lines: pub(crate) fn unlimited() -> Self {

**`mqtt-transport/src/session.rs`**
- DELETE: 18 lines: pub(crate) fn encrypt_next_with_direction_for_test(

**`server/src/agent/mod.rs`**
- DELETE: 8 lines: if request
- DELETE: 4 lines: if requested_focus.is_some_and(|focus| focus.contains("__test_fail_fal
- DELETE: 2 lines: wait_for_agent_startup_test_gate(&agent_id).await;
- DELETE: 2 lines: wait_for_agent_startup_selection_test_gate(&agent_id).await;
- DELETE: 5 lines: wait_for_append_supervisor_warning_test_gate(
- DELETE: 7 lines: let revalidation_forced = active
- UNGATE: let revalidation_forced = false;
- DELETE: 6 lines: struct AgentStartupTestGate {
- DELETE: 3 lines: static AGENT_STARTUP_TEST_GATE: std::sync::Mutex<Option<AgentStartupTe
- DELETE: 3 lines: static AGENT_STARTUP_SELECTION_TEST_GATE: std::sync::Mutex<Option<Agen
- DELETE: 3 lines: static COMPACT_IF_INACTIVE_TEST_GATE: std::sync::Mutex<Option<AgentSta
- DELETE: 6 lines: struct ContextFallbackTestGate {
- DELETE: 3 lines: static CONTEXT_FALLBACK_TEST_GATE: std::sync::Mutex<Option<ContextFall
- DELETE: 7 lines: fn begin_supervisor_verdict_test_gates()
- DELETE: 7 lines: fn append_supervisor_warning_test_gates()
- DELETE: 23 lines: pub(crate) fn install_begin_supervisor_verdict_test_gate(
- DELETE: 23 lines: pub(crate) fn install_append_supervisor_warning_test_gate(
- DELETE: 11 lines: async fn wait_for_begin_supervisor_verdict_test_gate(agent_id: &AgentI
- DELETE: 11 lines: async fn wait_for_append_supervisor_warning_test_gate(agent_id: &Agent
- UNGATE: async fn wait_for_begin_supervisor_verdict_test_gate(_agent_id: &Agent
- DELETE: 20 lines: pub(crate) fn install_compact_if_inactive_test_gate(
- DELETE: 4 lines: async fn wait_for_compact_if_inactive_test_gate(agent_id: &AgentId) {
- UNGATE: async fn wait_for_compact_if_inactive_test_gate(_agent_id: &AgentId) {
- DELETE: 20 lines: fn install_context_fallback_test_gate(
- DELETE: 20 lines: async fn wait_for_context_fallback_test_gate(session_id: &SessionId) {
- UNGATE: async fn wait_for_context_fallback_test_gate(_session_id: &SessionId)
- DELETE: 4 lines: async fn wait_for_agent_startup_test_gate(agent_id: &AgentId) {
- DELETE: 4 lines: async fn wait_for_agent_startup_selection_test_gate(agent_id: &AgentId
- DELETE: 18 lines: async fn wait_for_matching_agent_startup_test_gate(
- DELETE: 20 lines: fn attach_subscriber(

**`server/src/agent/registry.rs`**
- DELETE: 9 lines: pub fn new() -> (Self, watch::Receiver<u64>) {

**`server/src/code_intel/lsp_client.rs`**
- UNGATE: const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
- DELETE: 2 lines: const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
- UNGATE: const INITIALIZE_REQUEST_TIMEOUT: Duration = REQUEST_TIMEOUT;
- DELETE: 2 lines: const INITIALIZE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
- DELETE: 4 lines: PendingCount {
- DELETE: 8 lines: pub(crate) fn from_io<W, R>(
- DELETE: 6 lines: async fn pending_request_count(&self) -> usize {
- DELETE: 4 lines: pub(crate) fn child_slot(&self) -> Option<Arc<Mutex<Option<AsyncGroupC
- DELETE: 4 lines: LspCommand::PendingCount { reply } => {

**`server/src/code_intel/lsp_provider.rs`**
- DELETE: 8 lines: pub(crate) fn new(
- UNGATE: fn restart_backoff(attempt: u32) -> Duration {
- DELETE: 4 lines: fn restart_backoff(attempt: u32) -> Duration {

**`server/src/code_intel/service.rs`**
- DELETE: 4 lines: pub(crate) fn uses_project_handle_for_test(&self, handle: &ProjectStre

**`server/src/host.rs`**
- DELETE: 6 lines: struct SupervisorVerdictPostSampleTestGate {
- DELETE: 3 lines: static SUPERVISOR_VERDICT_POST_SAMPLE_TEST_GATE: StdMutex<
- DELETE: 20 lines: fn install_supervisor_verdict_post_sample_test_gate(
- DELETE: 17 lines: async fn wait_for_supervisor_verdict_post_sample_test_gate(agent_id: &
- UNGATE: async fn wait_for_supervisor_verdict_post_sample_test_gate(_agent_id:
- DELETE: 8 lines: struct SupervisorVerdictCallStart {
- DELETE: 6 lines: struct SupervisorVerdictCallTestGate {
- DELETE: 7 lines: fn supervisor_verdict_call_test_gates()
- DELETE: 23 lines: fn install_supervisor_verdict_call_test_gate(
- DELETE: 11 lines: fn remove_supervisor_verdict_call_test_gate(agent_id: &AgentId) {
- DELETE: 28 lines: async fn wait_for_supervisor_verdict_call_test_gate(
- UNGATE: async fn wait_for_supervisor_verdict_call_test_gate(
- DELETE: 4 lines: struct InstalledTeamMutationAfterRefsHook {
- DELETE: 7 lines: struct TeamMutationAfterRefsHook {
- DELETE: 2 lines: type TeamMutationAfterRefsHookCell = std::sync::Mutex<Option<Arc<TeamM
- DELETE: 10 lines: impl InstalledTeamMutationAfterRefsHook {
- DELETE: 15 lines: impl Drop for InstalledTeamMutationAfterRefsHook {
- DELETE: 18 lines: fn install_team_mutation_after_refs_test_hook(
- DELETE: 17 lines: async fn wait_for_team_mutation_after_refs_test_hook(host: &HostHandle
- DELETE: 5 lines: fn team_mutation_after_refs_hook_cell() -> &'static TeamMutationAfterR
- DELETE: 4 lines: struct InstalledSpawnSessionRegistrationHook {
- DELETE: 6 lines: struct SpawnSessionRegistrationHook {
- DELETE: 2 lines: type SpawnSessionRegistrationHookCell = std::sync::Mutex<Option<Arc<Sp
- DELETE: 10 lines: impl InstalledSpawnSessionRegistrationHook {
- DELETE: 15 lines: impl Drop for InstalledSpawnSessionRegistrationHook {
- DELETE: 19 lines: fn install_spawn_session_registration_test_hook(
- DELETE: 17 lines: async fn wait_for_spawn_session_registration_test_hook(host: &HostHand
- DELETE: 5 lines: fn spawn_session_registration_hook_cell() -> &'static SpawnSessionRegi
- DELETE: 4 lines: struct InstalledStartupFailureFanoutRaceHook {
- DELETE: 8 lines: struct StartupFailureFanoutRaceHook {
- DELETE: 6 lines: enum StartupFailureFanoutRaceWinner {
- DELETE: 2 lines: type StartupFailureFanoutRaceHookCell = std::sync::Mutex<Option<Arc<St
- DELETE: 6 lines: impl InstalledStartupFailureFanoutRaceHook {
- DELETE: 16 lines: impl Drop for InstalledStartupFailureFanoutRaceHook {
- DELETE: 22 lines: fn install_startup_failure_fanout_race_test_hook(
- DELETE: 19 lines: async fn wait_before_startup_failure_fanout_test_hook(host: &HostHandl
- DELETE: 12 lines: fn notify_startup_failure_fanout_claimed_test_hook(host: &HostHandle)
- DELETE: 12 lines: fn notify_startup_failure_claimed_test_hook(host: &HostHandle) {
- DELETE: 19 lines: async fn wait_for_startup_failure_fanout_race_test_hook(host: &HostHan
- DELETE: 5 lines: fn startup_failure_fanout_race_hook_cell() -> &'static StartupFailureF
- DELETE: 4 lines: struct InstalledSpawnNewAgentFanoutHook {
- DELETE: 7 lines: struct SpawnNewAgentFanoutHook {
- DELETE: 2 lines: type SpawnNewAgentFanoutHookCell = std::sync::Mutex<Option<Arc<SpawnNe
- DELETE: 6 lines: impl InstalledSpawnNewAgentFanoutHook {
- DELETE: 15 lines: impl Drop for InstalledSpawnNewAgentFanoutHook {
- DELETE: 18 lines: fn install_spawn_new_agent_fanout_test_hook(host: &HostHandle) -> Inst
- DELETE: 19 lines: async fn wait_after_spawn_new_agent_fanout_test_hook(host: &HostHandle
- DELETE: 5 lines: fn spawn_new_agent_fanout_hook_cell() -> &'static SpawnNewAgentFanoutH
- DELETE: 4 lines: struct InstalledSpawnVisibleBeforePublicationHook {
- DELETE: 6 lines: struct SpawnVisibleBeforePublicationHook {
- DELETE: 3 lines: type SpawnVisibleBeforePublicationHookCell =
- DELETE: 10 lines: impl InstalledSpawnVisibleBeforePublicationHook {
- DELETE: 15 lines: impl Drop for InstalledSpawnVisibleBeforePublicationHook {
- DELETE: 19 lines: fn install_spawn_visible_before_publication_test_hook(
- DELETE: 17 lines: async fn wait_for_spawn_visible_before_publication_test_hook(host: &Ho
- DELETE: 6 lines: fn spawn_visible_before_publication_hook_cell() -> &'static SpawnVisib
- DELETE: 4 lines: struct InstalledSpawnCancelledBeforeCleanupHook {
- DELETE: 6 lines: struct SpawnCancelledBeforeCleanupHook {
- DELETE: 3 lines: type SpawnCancelledBeforeCleanupHookCell =
- DELETE: 10 lines: impl InstalledSpawnCancelledBeforeCleanupHook {
- DELETE: 15 lines: impl Drop for InstalledSpawnCancelledBeforeCleanupHook {
- DELETE: 19 lines: fn install_spawn_cancelled_before_cleanup_test_hook(
- DELETE: 17 lines: async fn wait_for_spawn_cancelled_before_cleanup_test_hook(host: &Host
- DELETE: 6 lines: fn spawn_cancelled_before_cleanup_hook_cell() -> &'static SpawnCancell
- DELETE: 9 lines: pub(crate) async fn register_host_stream(
- DELETE: 5 lines: pub(crate) async fn spawn_agent(&self, payload: SpawnAgentPayload) ->
- DELETE: 16 lines: pub(crate) async fn compact_agent(
- DELETE: 2 lines: wait_for_spawn_session_registration_test_hook(self).await;
- DELETE: 2 lines: wait_before_startup_failure_fanout_test_hook(self).await;
- DELETE: 2 lines: notify_startup_failure_fanout_claimed_test_hook(self);
- DELETE: 2 lines: wait_after_spawn_new_agent_fanout_test_hook(self).await;
- DELETE: 2 lines: wait_for_spawn_visible_before_publication_test_hook(self).await;
- DELETE: 2 lines: wait_for_spawn_session_registration_test_hook(self).await;
- DELETE: 2 lines: wait_before_startup_failure_fanout_test_hook(self).await;
- DELETE: 2 lines: notify_startup_failure_fanout_claimed_test_hook(self);
- DELETE: 2 lines: wait_after_spawn_new_agent_fanout_test_hook(self).await;
- DELETE: 2 lines: wait_for_spawn_visible_before_publication_test_hook(self).await;
- DELETE: 2 lines: wait_for_team_mutation_after_refs_test_hook(self, operation).await;
- DELETE: 2 lines: wait_for_startup_failure_fanout_race_test_hook(&host).await;
- DELETE: 2 lines: notify_startup_failure_claimed_test_hook(&host);
- DELETE: 2 lines: wait_for_spawn_cancelled_before_cleanup_test_hook(self).await;
- DELETE: 3 lines: let transcript_store =
- UNGATE: spawn_agent_supervisor_task(host.clone());
- DELETE: 4 lines: if runtime_config.start_agent_supervisor_worker {
- DELETE: 6 lines: fn normalize_antigravity_session_resumability_with<F>(
- DELETE: 4 lines: fn antigravity_summary_is_permanently_non_resumable(session: &SessionS

**`server/src/project_stream.rs`**
- DELETE: 5 lines: pub(crate) fn disconnected_for_test() -> Self {
- DELETE: 4 lines: pub(crate) fn same_channel_for_test(&self, other: &Self) -> bool {

**`server/src/store/settings.rs`**
- DELETE: 4 lines: pub(crate) fn empty_settings_for_test() -> HostSettings {

**`server/src/store/transcript.rs`**
- DELETE: 2 lines: actor_io_enabled: bool,
- DELETE: 2 lines: actor_io_enabled: false,
- DELETE: 5 lines: pub(crate) fn with_actor_io_enabled(mut self, enabled: bool) -> Self {
- UNGATE: {
- DELETE: 4 lines: {
- DELETE: 16 lines: fn window_before(

**`server/src/stream.rs`**
- DELETE: 5 lines: pub fn depths(&self) -> (usize, usize, usize, usize) {

## Kept / restored for wasm UI tests

- `frontend/src/dispatch.rs` `mod restore_fixtures` — restored, re-gated to
  `#[cfg(all(test, target_arch = "wasm32"))]` (wasm lifecycle tests import it).
- `frontend/src/state.rs` `CenterZoneState::occurrences` — restored wasm-gated
  (used by file_explorer/git_panel wasm tests).
- `frontend/src/highlight_worker.rs` `ActiveTasks::contains_active` — restored
  wasm-gated.
- `mobile-frontend/src/dispatch.rs` `use PendingSessionHistoryRequest` —
  restored wasm-gated.
- `frontend/src/state.rs` composer-draft counters (`#[cfg(test)]` increments +
  `#[cfg(not(test))]` dialog twin) — kept untouched: wasm tests assert on them.
- All `test-support`-feature items (server, mqtt-transport) — kept: they are
  the e2e suite's plumbing, not unit tests.
- `vendor/webrtc-audio-processing-sys/build_support.rs` inline test mod (20
  tests) — vendored code, left in place; its repo-side runner
  `tests/tests/webrtc_build_support.rs` was deleted.
- `tools/test_dev_check.py`
  `test_cargo_build_uses_lazy_pinned_native_tool_wrapper` — the assertions
  pinning the deleted webrtc runner into existence were removed (the file it
  read no longer exists by policy); the rest of the contract test is intact.
