    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn run_with_context_blocks_when_budget_exceeded_pre_run() {
        use crate::engine::runner::{TaskRunner, agents};
        use crate::engine::router::RouteResult;
        use crate::engine::router::AgentProfile;
        
        let _guard = ENV_LOCK.lock().unwrap();
        let temp_home = tempfile::TempDir::new().unwrap();
        let orch_home = temp_home.path().join(".orch");
        std::fs::create_dir_all(&orch_home).unwrap();
        
        // Enable budget checks with a low limit
        std::fs::write(
            orch_home.join("config.yml"),
            "workflow:\n  max_tokens_per_task: 10\n",
        ).unwrap();
        
        let old_orch_home = std::env::var("ORCH_HOME").ok();
        std::env::set_var("ORCH_HOME", &orch_home);
        crate::config::reload().await;

        let repo = "owner/repo".to_string();
        let task_id = "111";

        // Create TaskStore
        let db_path = orch_home.join("orch.db");
        let store = Arc::new(crate::store::TaskStore::open_path(&db_path).await.unwrap());
        
        // Add task with a PR number
        let row_id = store.create_task(&repo, task_id, "Title", "Body", "owner").await.unwrap();
        store.update_task_fields(row_id, &[
            ("pr_number", serde_json::json!(42)),
        ]).await.unwrap();
        
        // Exceed the 10 token budget
        let run_id = store.start_run(&crate::store::StartRun {
            task_id: row_id,
            attempt: 1,
            run_type: "agent",
            agent: "claude",
            model: "haiku",
            command: "cmd",
            prompt: "prompt",
        }).await.unwrap();
        
        store.complete_run(&crate::store::CompleteRun {
            run_id,
            exit_code: Some(0),
            stdout: "",
            stderr: "",
            parsed: "",
            outcome: "success",
            error: "",
            tokens: crate::store::RunTokenUsage {
                input_tokens: Some(50),
                output_tokens: Some(50),
                total_cost_usd: 0.1,
                duration_secs: 1.0,
            },
        }).await.unwrap();
        
        let mut runner = TaskRunner::new(repo.clone());
        runner.store = Some(store.clone());
        let parent = make_task(task_id);
        let backend = Arc::new(TrackingBackend::new());
        let backend_dyn: Arc<dyn crate::backends::ExternalBackend> = backend.clone();
        let tmux = Arc::new(crate::tmux::TmuxManager::new());
        
        let rr = RouteResult {
            agent: "claude".to_string(),
            model: Some("haiku".to_string()),
            complexity: "simple".to_string(),
            reason: "".to_string(),
            profile: AgentProfile {
                role: "".to_string(),
                skills: vec![],
                tools: vec![],
                constraints: vec![],
            },
            selected_skills: vec![],
            warning: None,
        };
        
        let signal = runner.run_with_context(&parent, &backend_dyn, &tmux, Some(&rr)).await.unwrap();
        
        // It should block the task, not put it in needs_review, 
        // even though it has a PR
        assert!(matches!(signal, crate::engine::router::WeightSignal::Blocked));
        
        let updates = backend.status_updates.lock().unwrap();
        assert!(updates.iter().any(|(id, s)| id == "111" && *s == crate::backends::Status::Blocked));

        if let Some(old) = old_orch_home {
            std::env::set_var("ORCH_HOME", old);
        } else {
            std::env::remove_var("ORCH_HOME");
        }
    }
