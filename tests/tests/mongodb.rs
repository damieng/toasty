#![cfg(feature = "mongodb")]

use mongodb::{Client, bson::Document};
use toasty_driver_mongodb::MongoDb;

struct MongoDbSetup {
    url: String,
    db_name: String,
    client: Client,
    // Keeps MongoDB's SDAM background tasks alive for the duration of the test
    // suite. The tasks are spawned on this runtime when the Client is created;
    // dropping it would cancel them and leave all servers in "Unknown" state.
    _bg_runtime: tokio::runtime::Runtime,
}

impl MongoDbSetup {
    fn new() -> Self {
        let url = std::env::var("TOASTY_CONNECTION_URL").unwrap_or_else(|_| {
            "mongodb://localhost:27017/toasty_tests?directConnection=true".to_string()
        });

        let db_name = url::Url::parse(&url)
            .ok()
            .and_then(|u| {
                let path = u.path().trim_start_matches('/');
                (!path.is_empty()).then(|| path.to_string())
            })
            .unwrap_or_else(|| "toasty_tests".to_string());

        let (client, bg_runtime) = std::thread::spawn({
            let url = url.clone();
            move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let client = rt.block_on(Client::with_uri_str(&url)).unwrap();
                (client, rt)
            }
        })
        .join()
        .unwrap();

        MongoDbSetup {
            url,
            db_name,
            client,
            _bg_runtime: bg_runtime,
        }
    }
}

#[async_trait::async_trait]
impl toasty_driver_integration_suite::Setup for MongoDbSetup {
    fn driver(&self) -> Box<dyn toasty_core::driver::Driver> {
        Box::new(MongoDb::with_client(
            self.url.clone(),
            self.client.clone(),
            self.db_name.clone(),
        ))
    }

    async fn delete_table(&self, name: &str) {
        let _ = self
            .client
            .database(&self.db_name)
            .collection::<Document>(name)
            .drop()
            .await;
    }
}

// Generate all driver tests. MongoDB capability mirrors DynamoDB for now;
// tests requiring updates/deletes will fail until those operations are
// implemented in the driver.
toasty_driver_integration_suite::generate_driver_tests!(
    MongoDbSetup::new(),
    sql: false,
    auto_increment: false,
    bigdecimal_implemented: false,
    decimal_arbitrary_precision: false,
    native_decimal: false,
    native_varchar: false,
    native_ilike: false,
    native_timestamp: false,
    native_date: false,
    native_time: false,
    native_datetime: false,
    native_array: false,
    vec_scalar: true,
    vec_remove: false,
    vec_pop: false,
    vec_remove_at: false,
    backward_pagination: false,
    test_connection_pool: false,
    transaction_lock_mode: false,
);
