pub fn index(table: &str, columns: &[String], unique: bool) -> String {
    format!(
        "{}_{}_{}",
        table,
        columns.join("_"),
        if unique { "uq" } else { "idx" }
    )
}

pub fn foreign_key(table: &str, column: &str) -> String {
    format!("{table}_{column}_fk")
}

pub fn not_null_constraint(table: &str, column: &str) -> String {
    format!("{table}_{column}_not_null")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_the_catalog_contract() {
        assert_eq!(index("users", &["email".into()], true), "users_email_uq");
        assert_eq!(foreign_key("posts", "author"), "posts_author_fk");
        assert_eq!(
            not_null_constraint("users", "email"),
            "users_email_not_null"
        );
    }
}
