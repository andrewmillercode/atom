#[cfg(test)]
mod repro_real_cache {
    use crate::providers::modelsdev::*;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::providers::test_lock()
    }

    const NULL_FIXTURE: &[u8] = br#"{
      "null-provider": null,
      "null-everything": {"name":null,"api":null,"env":null,"doc":null,"models":null},
      "env-with-null": {"api":"https://x/v1","env":[null,"OPENAI_API_KEY",null],"models":{}},
      "models-map-with-null": {"api":"https://x/v1","models":{"bad":null,"good":{"reasoning":false}}},
      "reasoning-nulls": {"api":"https://x/v1","models":{"m":{
        "reasoning":true,
        "reasoning_options":[null,{"type":null,"values":null},{"type":"effort","values":[null,"high","max"]}],
        "limit":null
      }}},
      "ollama-cloud": {"api":"https://ollama.com/v1","models":{
        "deepseek-v4-flash:0731":{"reasoning":true,"reasoning_options":[{"type":"toggle"},{"type":"effort","values":["high","max"]}],"limit":{"context":1048576}}
      }}
    }"#;

    #[test]
    fn nulls_parse_like_go_encoding_json() {
        let cat = load_models_dev_catalog_bytes(NULL_FIXTURE).unwrap();

        assert!(cat.contains_key("null-provider"));
        assert!(cat.contains_key("null-everything"));

        let env = &cat["env-with-null"].env;
        assert_eq!(
            env,
            &["".to_string(), "OPENAI_API_KEY".to_string(), "".to_string()]
        );

        let m = &cat["models-map-with-null"].models;
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("bad"));
        assert!(m.contains_key("good"));

        let r = &cat["reasoning-nulls"].models["m"];
        assert!(r.reasoning);
        assert_eq!(r.reasoning_options.len(), 3);
        assert_eq!(r.reasoning_options[0], ModelsDevReasoningOpt::default());
        assert_eq!(r.reasoning_options[1], ModelsDevReasoningOpt::default());
        assert_eq!(
            r.reasoning_options[2].values,
            vec!["".to_string(), "high".to_string(), "max".to_string()]
        );
        assert_eq!(r.limit.context, 0);
    }

    #[test]
    fn null_fixture_lookup_context_window() {
        let _g = lock();
        let cat = load_models_dev_catalog_bytes(NULL_FIXTURE).unwrap();
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(
            context_window_tokens("ollama", "deepseek-v4-flash:0731"),
            1048576
        );
    }

    #[test]
    fn null_fixture_reasoning_levels() {
        let _g = lock();
        let cat = load_models_dev_catalog_bytes(NULL_FIXTURE).unwrap();
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(
            reasoning_levels_for("ollama", "deepseek-v4-flash:0731"),
            Some(vec![
                "none".to_string(),
                "high".to_string(),
                "max".to_string()
            ])
        );
    }

    #[test]
    fn whole_null_document_is_empty_catalog() {
        let cat = load_models_dev_catalog_bytes(b"null").unwrap();
        assert!(cat.is_empty());
    }

    #[test]
    fn parse_real_cache_file() {
        let _g = lock();
        let path = crate::session::store::data_dir().join("models.dev.json");
        if !path.exists() {
            eprintln!("cache not present at {:?}; skipping", path);
            return;
        }
        let b = std::fs::read(&path).unwrap();
        let cat = load_models_dev_catalog_bytes(&b).expect("real cache parses");
        assert!(!cat.is_empty(), "real cache is non-empty");
        assert!(cat.len() >= 193, "provider catalog unexpectedly shrank");
        assert!(catalog_has_api_metadata(&cat), "api metadata survives");
        let p = cat.get("ollama-cloud").expect("ollama-cloud present");
        let m = p
            .models
            .get("deepseek-v4-flash:0731")
            .expect("model present");
        assert_eq!(m.limit.context, 1048576);
        set_models_dev_catalog_for_test(Some(cat));
        assert_eq!(
            context_window_tokens("ollama", "deepseek-v4-flash:0731"),
            1048576
        );
        assert_eq!(
            reasoning_levels_for("ollama", "deepseek-v4-flash:0731"),
            Some(vec![
                "none".to_string(),
                "high".to_string(),
                "max".to_string()
            ])
        );
    }
}
