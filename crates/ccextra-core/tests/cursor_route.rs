use ccextra_core::route::{
    resolve_route_with_body, ModelConfig, Protocol, ProviderConfig, RouteError,
};
use serde_json::json;

fn provider(ids: &[&str]) -> ProviderConfig {
    let models = ids
        .iter()
        .map(|id| ModelConfig {
            name: (*id).into(),
            alias: (*id).into(),
            ..Default::default()
        })
        .collect();
    ProviderConfig::new(
        "cursor".into(),
        Protocol::Cursor,
        vec!["https://api2.cursor.sh".into()],
        "key".into(),
        None,
        false,
        models,
    )
    .with_metadata([("default_model".into(), "composer-2-high".into())].into())
}

#[test]
fn explicit_variants_must_be_advertised() {
    let providers = vec![provider(&["composer-2-medium", "composer-2-extra-high"])];
    let route = resolve_route_with_body("composer-2-extra-high", &json!({}), &providers).unwrap();
    assert_eq!(route.upstream_model, "composer-2-extra-high");
    let error = resolve_route_with_body("composer-2-high", &json!({}), &providers).unwrap_err();
    assert!(matches!(error, RouteError::VariantUnavailable(_)));
}

#[test]
fn family_selects_only_advertised_effort_thinking_and_speed() {
    let providers = vec![provider(&[
        "composer-2-medium-thinking",
        "composer-2-high-thinking-fast",
        "composer-2-extra-high-thinking",
        "composer-2-low",
    ])];
    let route = resolve_route_with_body("composer-2", &json!({}), &providers).unwrap();
    assert_eq!(route.upstream_model, "composer-2-medium-thinking");
    let route = resolve_route_with_body(
        "composer-2",
        &json!({
            "output_config": {"effort": "xhigh"}, "thinking": {"type": "enabled"}
        }),
        &providers,
    )
    .unwrap();
    assert_eq!(route.upstream_model, "composer-2-extra-high-thinking");
    let route = resolve_route_with_body(
        "composer-2",
        &json!({
            "thinking": {"type": "disabled"}, "reasoning_effort": "low"
        }),
        &providers,
    )
    .unwrap();
    assert_eq!(route.upstream_model, "composer-2-low");
    let route = resolve_route_with_body(
        "composer-2",
        &json!({
            "output_config": {"effort": "high"}, "service_tier": "priority"
        }),
        &providers,
    )
    .unwrap();
    assert_eq!(route.upstream_model, "composer-2-high-thinking-fast");
    assert!(matches!(
        resolve_route_with_body(
            "composer-2",
            &json!({
                "reasoning_effort": "max"
            }),
            &providers
        ),
        Err(RouteError::VariantUnavailable(_))
    ));
}

#[test]
fn auto_requires_catalog_entry_or_configured_default() {
    let providers = vec![provider(&["composer-2-high"])];
    assert_eq!(
        resolve_route_with_body("auto", &json!({}), &providers)
            .unwrap()
            .upstream_model,
        "composer-2-high"
    );
    let providers = vec![provider(&["auto", "composer-2-high"])];
    assert_eq!(
        resolve_route_with_body("auto", &json!({}), &providers)
            .unwrap()
            .upstream_model,
        "auto"
    );
    let providers = vec![provider(&["composer-2-medium"])];
    assert!(matches!(
        resolve_route_with_body("auto", &json!({}), &providers),
        Err(RouteError::VariantUnavailable(_)) | Err(RouteError::ModelNotFound(_))
    ));
}
