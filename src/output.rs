use serde::Serialize;
use tabled::{Table, Tabled};
use tabled::builder::Builder;

pub fn print_table<T: Tabled>(items: &[T]) {
    if items.is_empty() {
        println!("No results found.");
        return;
    }
    let table = Table::new(items);
    println!("{}", table);
}

/// Print a table with a footer row for totals.
/// footer_values should be a vec with values positioned at the correct column indices.
/// Empty strings for columns with no footer value.
pub fn print_table_with_footer<T: Tabled, S: AsRef<str>>(items: &[T], footer_values: Vec<S>) {
    if items.is_empty() {
        println!("No results found.");
        return;
    }

    // Build table manually to add footer row
    let mut builder = Builder::default();

    // Add header
    let headers: Vec<String> = T::headers().into_iter().map(|s| s.to_string()).collect();
    builder.push_record(&headers);

    // Add data rows
    for item in items {
        let fields: Vec<String> = item.fields().into_iter().map(|s| s.to_string()).collect();
        builder.push_record(&fields);
    }

    // Add footer row
    let footer_strs: Vec<&str> = footer_values.iter().map(|s| s.as_ref()).collect();
    builder.push_record(footer_strs);

    let table = builder.build();
    println!("{}", table);
}

pub fn print_json<T: Serialize>(items: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(items)?;
    println!("{}", json);
    Ok(())
}
