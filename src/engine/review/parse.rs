use super::{
    REVIEW_TIMEOUT_EXIT_CODE, ReviewContext, ReviewDecision, ReviewPhase, ReviewRun,
    complete_review_run, format_review_error, outcome_for_agent_error, output_has_review_decision,
    record_review_agent_failure, store_review_timeout,
};
/// Maximum consecutive review agent failures before the task is blocked
/// for human intervention. Exported so `tick` and `sync` use the same threshold.
use crate::backends::ExternalTask;
use crate::engine::cooldown;
use crate::engine::runner;
use crate::store;
use crate::store::RunTokenUsage;
use crate::store::TaskStore;
use crate::store::store_log_activity;
use std::fs;
use std::sync::Arc;
use tokio::task;

pub(super) struct ParsedReview {
    pub(super) run_id: Option<i64>,
    pub(super) exit_code: i32,
    pub(super) stderr: String,
    pub(super) raw_output: String,
    pub(super) text_for_review: String,
    pub(super) token_usage: RunTokenUsage,
    pub(super) review_notes_for_comment: String,
    pub(super) decision: ReviewDecision,
}

pub(super) fn review_decision_from_response(
    review_response: runner::response::ReviewResponse,
) -> ReviewDecision {
    match review_response.decision.as_str() {
        "approve" => approve_from_evidence(review_response),
        "request_changes" => ReviewDecision::RequestChanges {
            notes: review_response.notes,
            issues: review_response.issues,
        },
        _ => ReviewDecision::Failed(format!("unknown decision: {}", review_response.decision)),
    }
}

/// An approve that reports failing tests or error-severity issues is treated as request_changes
fn approve_from_evidence(review_response: runner::response::ReviewResponse) -> ReviewDecision {
    let tests_failed = review_response
        .test_results
        .as_deref()
        .is_some_and(|s| s.trim().eq_ignore_ascii_case("fail"));
    let has_error = review_response
        .issues
        .iter()
        .any(|issue| issue.severity.trim().eq_ignore_ascii_case("error"));
    if tests_failed || has_error {
        let notes = if tests_failed {
            notes_for_failed_tests(review_response.notes)
        } else {
            review_response.notes
        };
        ReviewDecision::RequestChanges {
            notes,
            issues: review_response.issues,
        }
    } else {
        ReviewDecision::Approve {
            issues: review_response.issues,
        }
    }
}

fn notes_for_failed_tests(notes: String) -> String {
    const MARK: &str = "Review reported test_results: fail";
    if notes.trim().is_empty() {
        MARK.to_string()
    } else if notes.contains("test_results: fail") {
        notes
    } else {
        format!("{notes}\n\n{MARK}")
    }
}

fn review_decision_name(decision: &ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approve { .. } => "approve",
        ReviewDecision::RequestChanges { .. } => "request_changes",
        ReviewDecision::Failed(_) => "failed",
        ReviewDecision::Blocked(_) => "blocked",
        ReviewDecision::Skipped => "skipped",
        ReviewDecision::Rerouted => "rerouted",
    }
}

pub(super) async fn parse_review_output(
    task: &ExternalTask,
    repo: &str,
    ctx: &ReviewContext,
    run: ReviewRun,
    store: &Arc<TaskStore>,
) -> ReviewPhase<ParsedReview> {
    let (file_exists, file_size, exit_code, stderr) = {
        let output_file_clone = ctx.output_file.clone();
        let review_attempt_dir_clone = ctx.review_attempt_dir.clone();
        match task::spawn_blocking(move || {
            let file_exists = output_file_clone.exists();
            let file_size = fs::metadata(&output_file_clone).map_or(0, |m| m.len());
            let exit_code = fs::read_to_string(review_attempt_dir_clone.join("exit.txt"))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .unwrap_or(-1);
            let stderr =
                fs::read_to_string(review_attempt_dir_clone.join("stderr.txt")).unwrap_or_default();
            (file_exists, file_size, exit_code, stderr)
        })
        .await
        {
            Ok(tuple) => tuple,
            Err(e) => {
                tracing::error!(
                    task_id = task.id.0,
                    error = %e,
                    "spawn_blocking panicked reading review output metadata"
                );
                (false, 0, -1, format!("spawn_blocking failed: {e}"))
            }
        }
    };

    tracing::info!(
        task_id = task.id.0,
        path = %ctx.output_file.display(),
        file_exists,
        file_size,
        "reading review agent output"
    );

    let raw_output =
        runner::response::read_output_file(&ctx.review_task_id, &ctx.output_file, repo).await;
    let raw_output = runner::agents::claude::strip_tty_control_lines(&raw_output);
    let has_result_envelope =
        runner::agents::claude::find_ndjson_result_line(&raw_output).is_some();
    let agent_result = runner::agents::find_agent_result(&ctx.review_agent, &raw_output);
    let token_usage = RunTokenUsage {
        input_tokens: agent_result
            .as_ref()
            .and_then(|r| r.input_tokens)
            .unwrap_or(0),
        output_tokens: agent_result
            .as_ref()
            .and_then(|r| r.output_tokens)
            .unwrap_or(0),
        total_cost_usd: agent_result
            .as_ref()
            .and_then(|r| r.cost_usd)
            .unwrap_or(0.0),
        duration_secs: 0.0,
    };

    let agent_result_is_error = agent_result.as_ref().is_some_and(|r| r.is_error);
    let ndjson_completed_no_error = raw_output.contains("\"terminal_reason\":\"completed\"")
        && !raw_output.contains("\"is_error\":true");
    // Do not short-circuit completed NDJSON sessions before rescue parsing.
    let is_hard_failure =
        !ndjson_completed_no_error && (raw_output.is_empty() || agent_result_is_error);

    if exit_code == REVIEW_TIMEOUT_EXIT_CODE
        && !output_has_review_decision(&ctx.review_agent, &raw_output)
    {
        tracing::error!(
            task_id = task.id.0,
            "review agent killed by its own timeout without a decision"
        );
        return store_review_timeout(store, run.run_id, exit_code, &raw_output, &stderr).await;
    }

    for window in runner::agents::claude::rate_limit_windows(&raw_output) {
        cooldown::record_rate_limit_window(&ctx.review_agent, &window).await;
    }

    if is_hard_failure {
        tracing::warn!(
            task_id = task.id.0,
            exit_code,
            raw_output_len = raw_output.len(),
            stderr_len = stderr.len(),
            "review agent: entering error path"
        );
        let agent_runner = runner::agents::get_runner(&ctx.review_agent);
        let err = agent_runner.classify_error(exit_code, &raw_output, &stderr);
        record_review_agent_failure(
            &task.id.0,
            &ctx.review_agent,
            ctx.review_model.as_deref(),
            &err,
        )
        .await;
        tracing::error!(task_id = task.id.0, error = %err, "review agent failed");
        let stored_error = format_review_error(&ctx.review_agent, &err);
        complete_review_run(
            store,
            run.run_id,
            Some(exit_code),
            &raw_output,
            &stderr,
            "",
            outcome_for_agent_error(&err),
            &stored_error,
            token_usage,
        )
        .await;
        return ReviewPhase::EarlyReturn(ReviewDecision::Failed(stored_error));
    }

    let text_for_review = agent_result
        .as_ref()
        .map(|r| r.result_text.clone())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            tracing::debug!(
                task_id = task.id.0,
                agent = %ctx.review_agent,
                "review agent: per-agent extractor returned no text, using raw output"
            );
            raw_output.clone()
        });

    let review_response = match runner::response::parse_review_response(&text_for_review) {
        Ok(response) => response,
        Err(e) => {
            if let Some(response) = runner::response::infer_review_response(&text_for_review) {
                tracing::warn!(
                    task_id = task.id.0,
                    agent = %ctx.review_agent,
                    "review response parsed via keyword fallback"
                );
                response
            } else {
                // Guard: if the raw NDJSON signals a completed session
                // (terminal_reason:completed without is_error:true), do NOT run
                // generic pattern-based rate-limit detection over it. Token
                // counts in the telemetry blob can contain bare "429" values
                // that would otherwise be misclassified as HTTP 429 errors.
                let ndjson_completed = ndjson_completed_no_error;

                // Missing type:result (kimi/minimax) or terminal_reason:completed: recover
                // from assistant text. Never MiniMax parse_response, which synthesizes status=done
                if ndjson_completed || !has_result_envelope {
                    let assistant_text = runner::agents::collect_assistant_messages_text(
                        &ctx.review_agent,
                        &raw_output,
                    );
                    if !assistant_text.is_empty() {
                        if let Ok(response) =
                            runner::response::parse_review_response(&assistant_text)
                        {
                            tracing::debug!(
                                task_id = task.id.0,
                                agent = %ctx.review_agent,
                                "found review response JSON in assistant message (terminal_reason:completed)"
                            );
                            let review_notes_for_comment = response.notes.clone();
                            let decision = review_decision_from_response(response);
                            return ReviewPhase::Ready(ParsedReview {
                                run_id: run.run_id,
                                exit_code,
                                stderr,
                                raw_output,
                                text_for_review: assistant_text,
                                token_usage,
                                review_notes_for_comment,
                                decision,
                            });
                        }
                        if let Some(response) =
                            runner::response::infer_review_response(&assistant_text)
                        {
                            tracing::warn!(
                                task_id = task.id.0,
                                agent = %ctx.review_agent,
                                "inferred review response from assistant message (terminal_reason:completed)"
                            );
                            let review_notes_for_comment = response.notes.clone();
                            let decision = review_decision_from_response(response);
                            return ReviewPhase::Ready(ParsedReview {
                                run_id: run.run_id,
                                exit_code,
                                stderr,
                                raw_output,
                                text_for_review: assistant_text,
                                token_usage,
                                review_notes_for_comment,
                                decision,
                            });
                        }
                    }
                }

                // Scan only bounded head/tail windows, not the full review text.
                // The full transcript may contain agent work product (e.g. a
                // diff or markdown report quoting an `outcome`/`rate_limit`
                // table cell) that incidentally matches these patterns; real
                // CLI errors appear at the start or end of the output. See
                // `classify_from_text` in runner::agents for the same rationale.
                let scan_head = runner::agents::patterns::safe_head(
                    &text_for_review,
                    runner::agents::patterns::RATE_LIMIT_SCAN_HEAD_BYTES,
                );
                let scan_tail = runner::agents::patterns::safe_tail(
                    &text_for_review,
                    runner::agents::patterns::RATE_LIMIT_SCAN_TAIL_BYTES,
                );

                if let Some(err) = (!ndjson_completed)
                    .then(|| {
                        runner::agents::patterns::detect_rate_limit(scan_head)
                            .or_else(|| runner::agents::patterns::detect_rate_limit(scan_tail))
                    })
                    .flatten()
                {
                    let runner::agents::AgentError::RateLimit { message } = &err else {
                        unreachable!("detect_rate_limit only returns AgentError::RateLimit")
                    };
                    tracing::warn!(
                        task_id = task.id.0,
                        agent = %ctx.review_agent,
                        "review agent hit rate limit — adding to cooldown"
                    );
                    // Route through record_review_agent_failure/outcome_for_agent_error
                    // (not the generic record_agent_failure_with_message + hardcoded
                    // "rate_limit" string) so billing-cycle exhaustion messages caught by
                    // this inline text scan get the same model-scoped persistent cooldown
                    // and outcome classification as the AgentError-based failure path
                    // above (issue #3551, gap left by #3530).
                    let summary = runner::fallback::summarize_rate_limit_error(message);
                    let stored_error = format!("{} rate limit: {summary}", ctx.review_agent);
                    record_review_agent_failure(
                        &task.id.0,
                        &ctx.review_agent,
                        ctx.review_model.as_deref(),
                        &err,
                    )
                    .await;
                    complete_review_run(
                        store,
                        run.run_id,
                        Some(exit_code),
                        &raw_output,
                        &stderr,
                        &text_for_review,
                        outcome_for_agent_error(&err),
                        &stored_error,
                        token_usage,
                    )
                    .await;
                    return ReviewPhase::EarlyReturn(ReviewDecision::Failed(stored_error));
                }

                if let Some(err) = runner::agents::patterns::detect_auth_error(scan_tail) {
                    let runner::agents::AgentError::Auth { message } = &err else {
                        unreachable!("detect_auth_error only returns AgentError::Auth")
                    };
                    tracing::warn!(
                        task_id = task.id.0,
                        agent = %ctx.review_agent,
                        "review agent hit auth error — adding to cooldown"
                    );
                    let stored_error = format!("auth error: {message}");
                    record_review_agent_failure(
                        &task.id.0,
                        &ctx.review_agent,
                        ctx.review_model.as_deref(),
                        &err,
                    )
                    .await;
                    complete_review_run(
                        store,
                        run.run_id,
                        Some(exit_code),
                        &raw_output,
                        &stderr,
                        &text_for_review,
                        outcome_for_agent_error(&err),
                        &stored_error,
                        token_usage,
                    )
                    .await;
                    return ReviewPhase::EarlyReturn(ReviewDecision::Failed(stored_error));
                }

                // Cut off at the output token limit (opencode step_finish reason=length)
                if agent_result.as_ref().is_some_and(|r| r.truncated_by_length) {
                    tracing::warn!(
                        task_id = task.id.0,
                        agent = %ctx.review_agent,
                        model = ?ctx.review_model,
                        "review agent output truncated: step_finish reason=length \
                         (token budget exceeded before completion)"
                    );
                    if let Some(model) = ctx.review_model.as_deref() {
                        cooldown::record_transient_model_failure(&ctx.review_agent, model).await;
                    }
                    let stored_error = "truncated: review output was cut off before completion \
                        (output/reasoning token budget exceeded)"
                        .to_string();
                    complete_review_run(
                        store,
                        run.run_id,
                        Some(exit_code),
                        &raw_output,
                        &stderr,
                        &text_for_review,
                        "truncated",
                        &stored_error,
                        token_usage,
                    )
                    .await;
                    return ReviewPhase::EarlyReturn(ReviewDecision::Failed(stored_error));
                }

                // No envelope and last content is thinking/tool_use: retry without
                // the format-failure cooldown (issue #3734)
                if runner::agents::claude::review_session_exited_mid_thought(&raw_output) {
                    tracing::warn!(
                        task_id = task.id.0,
                        agent = %ctx.review_agent,
                        model = ?ctx.review_model,
                        "review session exited mid-thought with no result envelope \
                         — storing as incomplete and retrying without persistent cooldown"
                    );
                    let stored_error =
                        "incomplete: review session exited before producing a decision".to_string();
                    complete_review_run(
                        store,
                        run.run_id,
                        Some(exit_code),
                        &raw_output,
                        &stderr,
                        &text_for_review,
                        "incomplete",
                        &stored_error,
                        token_usage,
                    )
                    .await;
                    return ReviewPhase::EarlyReturn(ReviewDecision::Failed(stored_error));
                }

                tracing::error!(
                    task_id = task.id.0,
                    error = %e,
                    agent = %ctx.review_agent,
                    output = %text_for_review.chars().take(300).collect::<String>(),
                    raw_output = %raw_output.chars().take(1000).collect::<String>(),
                    "failed to parse review response"
                );
                if let Some(model) = ctx.review_model.as_deref() {
                    cooldown::record_transient_model_failure(&ctx.review_agent, model).await;
                }
                complete_review_run(
                    store,
                    run.run_id,
                    Some(exit_code),
                    &raw_output,
                    &stderr,
                    &text_for_review,
                    "parse_error",
                    &format!("parse error: {e}"),
                    token_usage,
                )
                .await;
                return ReviewPhase::EarlyReturn(ReviewDecision::Failed(format!(
                    "parse error: {e}"
                )));
            }
        }
    };

    let review_notes_for_comment = review_response.notes.clone();
    let decision = review_decision_from_response(review_response);

    tracing::info!(
        task_id = task.id.0,
        pr_number = ctx.pr_number,
        decision = ?decision,
        "review agent decision received"
    );
    store_log_activity(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        "review_decision",
        None,
        None,
        Some(&ctx.review_agent),
        ctx.review_model.as_deref(),
        Some(&serde_json::json!({
            "decision": review_decision_name(&decision),
            "pr_number": ctx.pr_number,
        })),
    )
    .await;
    if let Err(e) = store::store_set_result(
        &Some(Arc::clone(store)),
        repo,
        &task.id.0,
        &[("review_resets", serde_json::json!(0))],
    )
    .await
    {
        tracing::warn!(task_id = task.id.0, err = %e, "failed to clear review_resets after a decision");
    }

    ReviewPhase::Ready(ParsedReview {
        run_id: run.run_id,
        exit_code,
        stderr,
        raw_output,
        text_for_review,
        token_usage,
        review_notes_for_comment,
        decision,
    })
}
