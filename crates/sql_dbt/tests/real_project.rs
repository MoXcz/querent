//! Loads a real dbt project: `DBT_PROJECT=/path/to/project cargo test -p sql_dbt -- --ignored`.

use sql_dbt::{Direction, ResourceType, SqlOrigin};

#[test]
#[ignore = "needs DBT_PROJECT pointing at a dbt project with target/manifest.json"]
fn loads_a_real_project_and_previews_every_model() {
    let root = std::env::var("DBT_PROJECT").expect("set DBT_PROJECT");
    let root = std::path::PathBuf::from(root);
    let found = sql_dbt::find_projects(std::slice::from_ref(&root), 4);
    assert_eq!(found, std::slice::from_ref(&root));
    let project = sql_dbt::load(&root).unwrap();
    let models: Vec<_> = project.nodes_of(ResourceType::Model).collect();
    assert_ne!(models.len(), 0);
    let (mut compiled, mut rendered, mut refused) = (0, 0, 0);
    for model in &models {
        match project.preview_sql(model) {
            Ok(preview) if preview.origin == SqlOrigin::Compiled => compiled += 1,
            Ok(_) => rendered += 1,
            Err(_) => refused += 1,
        }
        assert!(
            model.relation_name.is_some(),
            "{} has no relation",
            model.name
        );
    }
    let with_parents = models
        .iter()
        .filter(|model| {
            !project
                .lineage(&model.unique_id, Direction::Upstream)
                .is_empty()
        })
        .count();
    eprintln!(
        "{}: {} models ({compiled} compiled, {rendered} rendered, {refused} need compile), {with_parents} with parents, {} stale, catalog: {}",
        project.name,
        models.len(),
        project.stale.len(),
        project.has_catalog,
    );
    assert!(with_parents > 0);
}
