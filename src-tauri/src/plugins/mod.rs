//! Plugin code. Files in this folder may only use `crate::plugin_api` from the
//! app; the test below keeps that boundary.

mod vocabulary_coach;

use std::sync::Arc;

use crate::plugin_api::Plugin;

pub fn all() -> Vec<Arc<dyn Plugin>> {
    vec![Arc::new(vocabulary_coach::VocabularyCoach)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_ids_are_unique() {
        let plugins = all();
        let mut ids: Vec<_> = plugins.iter().map(|plugin| plugin.manifest().id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), plugins.len());
    }

    #[test]
    fn plugins_only_use_the_plugin_api() {
        // Built from parts so this test does not match its own pattern.
        let app = concat!("crate", "::");
        let allowed = concat!("crate", "::plugin_api");
        let folder = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/plugins");
        for entry in std::fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            let source = std::fs::read_to_string(&path).unwrap();
            for (index, _) in source.match_indices(app) {
                assert!(
                    source[index..].starts_with(allowed),
                    "{} uses app code outside crate::plugin_api",
                    path.display()
                );
            }
        }
    }
}
