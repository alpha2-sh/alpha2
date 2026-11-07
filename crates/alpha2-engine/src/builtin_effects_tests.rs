//! The model validates documents against the built-ins the renderer registers.

#[test]
fn the_model_embeds_the_catalog_the_renderer_publishes() {
    assert!(
        alpha2_model::builtin_effects_json() == alpha2_render::builtin_catalog_document(),
        "crates/alpha2-model/src/builtin-effects.json is stale; run `npm run generate:schema`"
    );
}
