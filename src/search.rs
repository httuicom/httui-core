use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    pub score: i32,
}

/// Fuzzy search files by name using subsequence matching with scoring.
pub fn search_files(vault_path: &str, query: &str) -> Result<Vec<SearchResult>, String> {
    let root = Path::new(vault_path);
    if !root.is_dir() {
        return Err("Vault path is not a directory".to_string());
    }

    let query_lower = query.to_lowercase();
    let mut results: Vec<SearchResult> = Vec::new();
    collect_md_files(root, root, &query_lower, &mut results)?;

    // Sort by score descending
    results.sort_by(|a, b| b.score.cmp(&a.score));
    results.truncate(20);
    Ok(results)
}

fn collect_md_files(
    dir: &Path,
    root: &Path,
    query: &str,
    results: &mut Vec<SearchResult>,
) -> Result<(), String> {
    let read_dir = std::fs::read_dir(dir).map_err(|e| e.to_string())?;

    for entry in read_dir {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if name.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            collect_md_files(&path, root, query, results)?;
        } else if name.ends_with(".md") {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();

            let display_name = name.trim_end_matches(".md").to_string();

            if query.is_empty() {
                results.push(SearchResult {
                    path: relative,
                    name: display_name,
                    score: 0,
                });
            } else if let Some(score) = fuzzy_score(&display_name.to_lowercase(), query) {
                results.push(SearchResult {
                    path: relative,
                    name: display_name,
                    score,
                });
            }
        }
    }
    Ok(())
}

/// Simple fuzzy subsequence matching with scoring.
/// Returns Some(score) if query is a subsequence of target, None otherwise.
fn fuzzy_score(target: &str, query: &str) -> Option<i32> {
    let target_chars: Vec<char> = target.chars().collect();
    let query_chars: Vec<char> = query.chars().collect();

    let mut qi = 0;
    let mut score: i32 = 0;
    let mut prev_match = false;

    for (ti, &tc) in target_chars.iter().enumerate() {
        if qi < query_chars.len() && tc == query_chars[qi] {
            qi += 1;
            // Bonus for consecutive matches
            if prev_match {
                score += 5;
            }
            // Bonus for matching at start
            if ti == 0 {
                score += 10;
            }
            // Bonus for matching after separator
            if ti > 0 && matches!(target_chars[ti - 1], '-' | '_' | ' ' | '/') {
                score += 8;
            }
            score += 1;
            prev_match = true;
        } else {
            prev_match = false;
        }
    }

    if qi == query_chars.len() {
        Some(score)
    } else {
        None
    }
}

// --- Full-text search (FTS5) ---

#[derive(Debug, Serialize)]
pub struct ContentSearchResult {
    pub file_path: String,
    pub snippet: String,
}

pub async fn rebuild_search_index(
    pool: &sqlx::sqlite::SqlitePool,
    vault_path: &str,
) -> Result<(), String> {
    sqlx::query("DELETE FROM search_index")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // Collect all .md files (sync)
    let root = Path::new(vault_path);
    let mut files: Vec<(String, String, String)> = Vec::new();
    collect_md_files_for_index(root, root, &mut files)?;

    // Index all (async)
    for (relative, title, content) in &files {
        sqlx::query(
            "INSERT OR REPLACE INTO search_index (file_path, title, content) VALUES (?1, ?2, ?3)",
        )
        .bind(relative)
        .bind(title)
        .bind(content)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn collect_md_files_for_index(
    dir: &Path,
    root: &Path,
    files: &mut Vec<(String, String, String)>,
) -> Result<(), String> {
    let read_dir = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    for entry in read_dir {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_md_files_for_index(&path, root, files)?;
        } else if name.ends_with(".md") {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let title = name.trim_end_matches(".md").to_string();
            files.push((relative, title, content));
        }
    }
    Ok(())
}

pub async fn update_search_entry(
    pool: &sqlx::sqlite::SqlitePool,
    file_path: &str,
    content: &str,
) -> Result<(), String> {
    let title = file_path
        .split('/')
        .next_back()
        .unwrap_or(file_path)
        .trim_end_matches(".md");

    sqlx::query(
        "INSERT OR REPLACE INTO search_index (file_path, title, content) VALUES (?1, ?2, ?3)",
    )
    .bind(file_path)
    .bind(title)
    .bind(content)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

pub async fn remove_search_entry(
    pool: &sqlx::sqlite::SqlitePool,
    file_path: &str,
) -> Result<(), String> {
    sqlx::query("DELETE FROM search_index WHERE file_path = ?1")
        .bind(file_path)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn search_content(
    pool: &sqlx::sqlite::SqlitePool,
    query: &str,
) -> Result<Vec<ContentSearchResult>, String> {
    if query.trim().is_empty() {
        return Ok(vec![]);
    }

    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT file_path, snippet(search_index, 2, '<mark>', '</mark>', '...', 32) \
         FROM search_index WHERE search_index MATCH ?1 ORDER BY rank LIMIT 50",
    )
    .bind(query)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(rows
        .into_iter()
        .map(|(file_path, snippet)| ContentSearchResult {
            file_path,
            snippet,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_vault() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("README.md"), "# Hello").unwrap();
        std::fs::write(root.join("notes.md"), "Notes").unwrap();
        std::fs::create_dir_all(root.join("subfolder")).unwrap();
        std::fs::write(root.join("subfolder/deep-note.md"), "Deep").unwrap();
        tmp
    }

    #[test]
    fn test_search_empty_query_returns_all() {
        let tmp = setup_vault();
        let results = search_files(tmp.path().to_str().unwrap(), "").unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_search_fuzzy_match() {
        let tmp = setup_vault();
        let results = search_files(tmp.path().to_str().unwrap(), "read").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "README");
    }

    #[test]
    fn test_search_no_match() {
        let tmp = setup_vault();
        let results = search_files(tmp.path().to_str().unwrap(), "xyz").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_subsequence() {
        let tmp = setup_vault();
        let results = search_files(tmp.path().to_str().unwrap(), "dn").unwrap();
        // "deep-note" matches d...n
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "deep-note");
    }
}
