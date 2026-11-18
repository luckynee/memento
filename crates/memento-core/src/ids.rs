use uuid::Uuid;

pub const POINT_NAMESPACE: Uuid = Uuid::from_u128(0xd5678f29_5487_5c3d_941b_16c39ae92be3);

pub fn point_id(harness: &str, session: &str, source_id: &str, chunk_index: u32) -> Uuid {
    let name = format!("{harness}/{session}/{source_id}/{chunk_index}");
    Uuid::new_v5(&POINT_NAMESPACE, name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_ids_are_deterministic_and_depend_on_all_fields() {
        let id = point_id("h", "s", "i", 0);
        assert_eq!(id, point_id("h", "s", "i", 0));
        assert_ne!(id, point_id("other", "s", "i", 0));
        assert_ne!(id, point_id("h", "other", "i", 0));
        assert_ne!(id, point_id("h", "s", "other", 0));
        assert_ne!(id, point_id("h", "s", "i", 1));
        assert_eq!(id.get_version_num(), 5);
    }
}
