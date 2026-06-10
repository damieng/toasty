#![cfg(feature = "mongodb")]

use mongodb::{Client, bson::Document};
use std::sync::OnceLock;
use toasty_driver_mongodb::MongoDb;

struct MongoDbSetup {
    url: String,
    db_name: String,
    client: OnceLock<Client>,
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

        MongoDbSetup {
            url,
            db_name,
            client: OnceLock::new(),
        }
    }

    fn get_client(&self) -> &Client {
        let url = self.url.clone();
        self.client.get_or_init(|| {
            std::thread::spawn(move || {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(Client::with_uri_str(&url))
                    .unwrap()
            })
            .join()
            .unwrap()
        })
    }
}

#[async_trait::async_trait]
impl toasty_driver_integration_suite::Setup for MongoDbSetup {
    fn driver(&self) -> Box<dyn toasty_core::driver::Driver> {
        let url = self.url.clone();
        let driver = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(MongoDb::new(url))
                .unwrap()
        })
        .join()
        .unwrap();
        Box::new(driver)
    }

    async fn delete_table(&self, name: &str) {
        let client = self.get_client();
        let _ = client
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
