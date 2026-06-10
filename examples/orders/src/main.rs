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

    let provider = match url.split(':').next().unwrap_or("") {
        "postgresql" | "postgres" => "PostgreSQL",
        "mysql" => "MySQL",
        "sqlite" => "SQLite",
        "dynamodb" => "DynamoDB",
        "mongodb" | "mongodb+srv" => "MongoDB",
        other => other,
    };

    println!("Provider: {provider}");
    println!("URL:      {url}");

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

    let banner_width = merged_width(&COL_WIDTHS);

    println!();
    println!("{}", rule(&[banner_width], '┌', &[], '┐'));
    println!("{}", row(&[pad("Orders", banner_width, Align::Left)]));
    println!("{}", rule(&[banner_width], '└', &[], '┘'));

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

        println!("{}", rule(&COL_WIDTHS, '┌', &['┬', '┬', '┬', '┬'], '┐'));
        println!(
            "{}",
            row(&[
                pad("Product ID", COL_WIDTHS[0], Align::Left),
                pad("Description", COL_WIDTHS[1], Align::Left),
                pad("Qty", COL_WIDTHS[2], Align::Right),
                pad("Unit", COL_WIDTHS[3], Align::Left),
                pad("Subtotal", COL_WIDTHS[4], Align::Left),
            ])
        );
        println!("{}", rule(&COL_WIDTHS, '├', &['┼', '┼', '┼', '┼'], '┤'));

        for d in &details {
            let subtotal = d.quantity as i64 * d.unit_price_cents;
            let product = format!("{:.8}…", &d.product_id.to_string()[..8]);
            println!(
                "{}",
                row(&[
                    pad(&product, COL_WIDTHS[0], Align::Left),
                    pad(&d.description, COL_WIDTHS[1], Align::Left),
                    pad(&d.quantity.to_string(), COL_WIDTHS[2], Align::Right),
                    pad(
                        &format_price(d.unit_price_cents),
                        COL_WIDTHS[3],
                        Align::Right
                    ),
                    pad(&format_price(subtotal), COL_WIDTHS[4], Align::Right),
                ])
            );
        }

        // The total row merges the first four columns into a single label cell,
        // keeping the subtotal aligned under the line-item subtotals above. The
        // merged width is derived from the column widths so it never drifts.
        let label_width = merged_width(&COL_WIDTHS[..4]);
        println!("{}", rule(&COL_WIDTHS, '├', &['┴', '┴', '┴', '┼'], '┤'));
        println!(
            "{}",
            row(&[
                pad("Total", label_width, Align::Right),
                pad(&format_price(order_total), COL_WIDTHS[4], Align::Right),
            ])
        );
        println!("{}", rule(&COL_WIDTHS, '└', &['─', '─', '─', '┴'], '┘'));
    }

    println!();
    Ok(())
}

fn format_price(cents: i64) -> String {
    format!("${}.{:02}", cents / 100, cents.unsigned_abs() % 100)
}

// ── Box-drawing helpers ──────────────────────────────────────────────────────
//
// The line-item table is drawn from a single source of truth — `COL_WIDTHS` —
// so borders, headers, rows, and the merged "Total" cell always line up. To
// change a column width, edit `COL_WIDTHS`; everything else follows.

/// Inner content widths of the line-item table columns
/// (product id, description, qty, unit price, subtotal).
const COL_WIDTHS: [usize; 5] = [26, 28, 4, 9, 9];

/// Indent applied to every line of the table.
const INDENT: &str = "  ";

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

/// Truncates `text` to `width` characters, then pads it to `width`.
fn pad(text: &str, width: usize, align: Align) -> String {
    let text: String = text.chars().take(width).collect();
    match align {
        Align::Left => format!("{text:<width$}"),
        Align::Right => format!("{text:>width$}"),
    }
}

/// Builds a horizontal rule. `junctions` supplies one character per internal
/// column boundary (length must be `widths.len() - 1`); pass `'─'` to let the
/// rule run straight through a boundary that has no vertical line above it.
fn rule(widths: &[usize], left: char, junctions: &[char], right: char) -> String {
    let mut line = String::from(INDENT);
    line.push(left);
    for (i, &width) in widths.iter().enumerate() {
        line.push_str(&"─".repeat(width + 2));
        if i + 1 < widths.len() {
            line.push(junctions[i]);
        }
    }
    line.push(right);
    line
}

/// Builds a content row. Each cell must already be padded to its column width;
/// the row adds the single-space gutters and vertical separators around them.
fn row(cells: &[String]) -> String {
    let mut line = String::from(INDENT);
    line.push('│');
    for cell in cells {
        line.push(' ');
        line.push_str(cell);
        line.push(' ');
        line.push('│');
    }
    line
}

/// Inner width of a single cell that spans the given adjacent columns,
/// reclaiming the gutters and separators the merged columns no longer need.
fn merged_width(cols: &[usize]) -> usize {
    let cells: usize = cols.iter().map(|w| w + 2).sum();
    cells + (cols.len() - 1) - 2
}
