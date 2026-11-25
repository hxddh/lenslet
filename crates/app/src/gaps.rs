//! What AWS Emulator is known not to do yet, from its own issue list: for a
//! failed call, whether the emulator, rather than your code, may be why.

include!(concat!(env!("OUT_DIR"), "/gaps.rs"));

/// The known gaps of a call's operation.
#[derive(Clone, Debug, PartialEq)]
pub struct Known {
    /// One of them names the very error the call got.
    pub exact: bool,
    /// The most telling one: the exact one, else the most severe.
    pub title: &'static str,
    /// Its issue in the emulator's repository.
    pub url: String,
    /// How many issues the operation has.
    pub count: usize,
}

/// The emulator's known gaps in `operation` of `service` (Lenslet's names),
/// for a call that failed with `code`.
pub fn known(service: &str, operation: &str, code: Option<&str>) -> Option<Known> {
    let start = GAPS.partition_point(|g| (g.0, g.1) < (service, operation));
    let rows: Vec<_> = GAPS[start..]
        .iter()
        .take_while(|g| g.0 == service && g.1 == operation)
        .collect();
    let exact =
        |g: &&(&str, &str, &str, u8, &str, &str)| code.is_some_and(|c| !g.2.is_empty() && g.2 == c);
    let best = rows.iter().min_by_key(|g| (!exact(g), g.3))?;
    let mut files: Vec<_> = rows.iter().map(|g| g.5).collect();
    files.sort_unstable();
    files.dedup();
    Some(Known {
        exact: exact(best),
        title: best.4,
        url: format!(
            "https://github.com/aws/aws-emulator/blob/{COMMIT}/issues/things-that-do-not-work/{}",
            best.5
        ),
        count: files.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_sorted_and_finds_what_it_holds() {
        assert!(
            GAPS.windows(2)
                .all(|w| (w[0].0, w[0].1) <= (w[1].0, w[1].1))
        );
        if let Some(g) = GAPS.first() {
            let found = known(g.0, g.1, Some(g.2)).expect("listed");
            assert!(found.count >= 1);
            assert!(found.url.ends_with(".md"));
        }
        assert_eq!(known("s3", "NoSuchOperationAnywhere", None), None);
    }
}
