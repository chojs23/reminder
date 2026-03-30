use chrono::{DateTime, Local, Utc};

pub(in crate::app) fn format_local_timestamp(datetime: DateTime<Utc>, format: &str) -> String {
    datetime.with_timezone(&Local).format(format).to_string()
}

#[cfg(test)]
mod tests {
    use chrono::{Local, TimeZone, Utc};

    use super::format_local_timestamp;

    #[test]
    fn format_local_timestamp_uses_local_timezone() {
        let utc = Utc
            .with_ymd_and_hms(2024, 1, 2, 3, 4, 5)
            .single()
            .expect("valid UTC timestamp");

        assert_eq!(
            format_local_timestamp(utc, "%Y-%m-%d %H:%M:%S %:z"),
            utc.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        );
    }
}
