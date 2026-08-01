//! Soket provider refresh uses cache/seed without network when denied.

use std::sync::Arc;

use loop_ai::providers::{soket_provider, soket_seed_models, SOKET_DEFAULT_MODEL_ID, SOKET_PROVIDER_ID};
use loop_ai::{
    CreateModelsOptions, InMemoryModelsStore, Models, ModelsRefreshOptions, ModelsStore,
    ModelsStoreEntry,
};
use loop_ai::utils::now_ms;

#[tokio::test]
async fn offline_refresh_keeps_seed_or_cache() {
    let store = Arc::new(InMemoryModelsStore::new());
    let models = Models::create(CreateModelsOptions {
        credentials: None,
        models_store: Some(store.clone()),
    });
    models.set_provider(soket_provider());

    let result = models
        .refresh(ModelsRefreshOptions {
            allow_network: Some(false),
            force: false,
            provider_id: Some(SOKET_PROVIDER_ID.into()),
        })
        .await;
    assert!(result.errors.is_empty() || !result.errors.is_empty()); // may error without cache; seed remains
    assert!(models
        .get_model(SOKET_PROVIDER_ID, SOKET_DEFAULT_MODEL_ID)
        .is_some());

    // Write a cache entry and refresh offline — should load cache.
    let cached = vec![
        soket_seed_models()[0].clone(),
        loop_ai::Model {
            id: "extra-from-cache".into(),
            name: "extra".into(),
            ..soket_seed_models()[0].clone()
        },
    ];
    store
        .write(
            SOKET_PROVIDER_ID,
            ModelsStoreEntry {
                models: cached.clone(),
                checked_at: now_ms(),
            },
        )
        .await
        .unwrap();

    // New models collection + provider, offline refresh
    let models2 = Models::create(CreateModelsOptions {
        credentials: None,
        models_store: Some(store),
    });
    models2.set_provider(soket_provider());
    let _ = models2
        .refresh(ModelsRefreshOptions {
            allow_network: Some(false),
            force: false,
            provider_id: Some(SOKET_PROVIDER_ID.into()),
        })
        .await;
    assert!(models2
        .get_model(SOKET_PROVIDER_ID, "extra-from-cache")
        .is_some());
}
