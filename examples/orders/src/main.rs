use uuid::Uuid;

#[derive(Debug, toasty::Embed)]
struct ShippingAddress {
    street: String,
    city: String,
    state: String,
    zip: String,
    country: String,
}

#[derive(Debug, toasty::Model)]
struct Order {
    #[key]
    #[auto]
    id: Uuid,

    customer_id: Uuid,

    status: String,

    shipping_address: ShippingAddress,

    #[has_many]
    details: toasty::Deferred<Vec<OrderDetail>>,
}

#[derive(Debug, toasty::Model)]
struct OrderDetail {
    #[key]
    #[auto]
    id: Uuid,

    #[index]
    order_id: Uuid,

    #[belongs_to(key = order_id, references = id)]
    order: toasty::Deferred<Order>,

    product_id: Uuid,

    description: String,

    quantity: i32,

    /// Unit price in cents
    unit_price_cents: i64,
}

#[tokio::main]
async fn main() -> toasty::Result<()> {
    let url = std::env::var("TOASTY_CONNECTION_URL")
        .unwrap_or_else(|_| "postgresql://toasty:toasty@localhost:5434/orders".to_string());

    let mut db = toasty::Db::builder()
        .models(toasty::models!(crate::*))
        .connect(&url)
        .await?;

    // Create tables if they don't exist; "already exists" is fine on re-runs.
    if let Err(e) = db.push_schema().await {
        if !e.to_string().contains("already exists") {
            return Err(e);
        }
    }

    // Seed only when the orders table is empty.
    if Order::all().exec(&mut db).await?.is_empty() {
        let customer_a = Uuid::new_v4();
        let customer_b = Uuid::new_v4();

        let widget_id = Uuid::new_v4();
        let gadget_id = Uuid::new_v4();
        let doohickey_id = Uuid::new_v4();
        let thingamajig_id = Uuid::new_v4();
        let whatsit_id = Uuid::new_v4();

        // Order 1 – two lines, shipped to New York
        toasty::create!(Order {
            customer_id: customer_a,
            status: "shipped",
            shipping_address: ShippingAddress {
                street: "742 Evergreen Terrace".to_string(),
                city: "New York".to_string(),
                state: "NY".to_string(),
                zip: "10001".to_string(),
                country: "US".to_string(),
            },
            details: [
                {
                    product_id: widget_id,
                    description: "Premium Widget (Blue)",
                    quantity: 2,
                    unit_price_cents: 1999,
                },
                {
                    product_id: gadget_id,
                    description: "Smart Gadget Pro",
                    quantity: 1,
                    unit_price_cents: 4999,
                },
            ],
        })
        .exec(&mut db)
        .await?;

        // Order 2 – three lines, pending delivery to Austin
        toasty::create!(Order {
            customer_id: customer_b,
            status: "pending",
            shipping_address: ShippingAddress {
                street: "1600 Barton Springs Rd".to_string(),
                city: "Austin".to_string(),
                state: "TX".to_string(),
                zip: "78704".to_string(),
                country: "US".to_string(),
            },
            details: [
                {
                    product_id: doohickey_id,
                    description: "Deluxe Doohickey (5-pack)",
                    quantity: 5,
                    unit_price_cents: 799,
                },
                {
                    product_id: thingamajig_id,
                    description: "Thingamajig XL",
                    quantity: 1,
                    unit_price_cents: 2499,
                },
                {
                    product_id: widget_id,
                    description: "Premium Widget (Blue)",
                    quantity: 3,
                    unit_price_cents: 1999,
                },
            ],
        })
        .exec(&mut db)
        .await?;

        // Order 3 – one line, delivered to London
        toasty::create!(Order {
            customer_id: customer_a,
            status: "delivered",
            shipping_address: ShippingAddress {
                street: "221B Baker Street".to_string(),
                city: "London".to_string(),
                state: "England".to_string(),
                zip: "NW1 6XE".to_string(),
                country: "GB".to_string(),
            },
            details: [
                {
                    product_id: whatsit_id,
                    description: "Whatsit Multipurpose Tool",
                    quantity: 10,
                    unit_price_cents: 349,
                },
            ],
        })
        .exec(&mut db)
        .await?;
    }

    // ── Query & display ────────────────────────────────────────────────────────

    let orders = Order::all().exec(&mut db).await?;

    println!();
    println!(
        "┌──────────────────────────────────────────────────────────────────────────────────┐"
    );
    println!(
        "│  Orders                                                                          │"
    );
    println!(
        "└──────────────────────────────────────────────────────────────────────────────────┘"
    );

    for order in &orders {
        let details = order.details().exec(&mut db).await?;
        let order_total: i64 = details
            .iter()
            .map(|d| d.quantity as i64 * d.unit_price_cents)
            .sum();
        let addr = &order.shipping_address;

        println!();
        println!("  Order    {}", order.id);
        println!("  Status   {}", order.status);
        println!("  Customer {}", order.customer_id);
        println!(
            "  Ship to  {}, {}, {} {}  {}",
            addr.street, addr.city, addr.state, addr.zip, addr.country
        );
        println!(
            "  ┌────────────────────────────┬──────────────────────────────┬──────┬───────────┬───────────┐"
        );
        println!(
            "  │ Product ID                 │ Description                  │  Qty │ Unit      │ Subtotal  │"
        );
        println!(
            "  ├────────────────────────────┼──────────────────────────────┼──────┼───────────┼───────────┤"
        );
        for d in &details {
            let subtotal = d.quantity as i64 * d.unit_price_cents;
            println!(
                "  │ {:26.26} │ {:<28.28} │ {:>4} │ {:>9} │ {:>9} │",
                format!("{:.8}…", &d.product_id.to_string()[..8]),
                d.description,
                d.quantity,
                format_price(d.unit_price_cents),
                format_price(subtotal),
            );
        }
        println!(
            "  ├────────────────────────────┴──────────────────────────────┴──────┴───────────┼───────────┤"
        );
        println!(
            "  │                                                                           Total │ {:>9} │",
            format_price(order_total)
        );
        println!(
            "  └────────────────────────────────────────────────────────────────────────────────┴───────────┘"
        );
    }

    println!();
    Ok(())
}

fn format_price(cents: i64) -> String {
    format!("${}.{:02}", cents / 100, cents.unsigned_abs() % 100)
}
