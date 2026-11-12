//! Минимальный Okapi BM25 (k1=1.2, b=0.75). Самописный: корпус — сотни документов
//! (каталог MCP-тулов, ноды wiki); tantivy и крейт `bm25` отвергнуты (PLAN §4).
//! Индекс строится один раз и кэшируется по generation вызывающей стороной —
//! никаких пересборок на запрос (анти-паттерн grok-build, PLAN §1.2).

use std::collections::HashMap;

const K1: f32 = 1.2;
const B: f32 = 0.75;

/// Токенизация идентификаторов: границы `_`, `-`, не-алфанумерика и camelCase.
/// `SearchDashboards` → `search dashboards`, `grafana-ai` → `grafana ai` (паттерн grok-build).
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower && !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = c.is_lowercase() || c.is_numeric();
            for lc in c.to_lowercase() {
                cur.push(lc);
            }
        } else {
            prev_lower = false;
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

#[derive(Debug, Default)]
pub struct Bm25Index {
    /// term -> [(doc, `term_freq`)]
    postings: HashMap<String, Vec<(u32, u32)>>,
    doc_lens: Vec<u32>,
    avg_len: f32,
}

impl Bm25Index {
    /// Строит индекс; документ идентифицируется своим порядковым номером.
    #[must_use]
    pub fn build<'a>(docs: impl IntoIterator<Item = &'a str>) -> Self {
        let mut postings: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        let mut doc_lens = Vec::new();
        for (doc_id, text) in (0u32..).zip(docs) {
            let tokens = tokenize(text);
            #[allow(clippy::cast_possible_truncation)] // документы << 4 млрд токенов
            doc_lens.push(tokens.len() as u32);
            let mut tf: HashMap<String, u32> = HashMap::new();
            for t in tokens {
                *tf.entry(t).or_insert(0) += 1;
            }
            for (term, freq) in tf {
                postings.entry(term).or_default().push((doc_id, freq));
            }
        }
        #[allow(clippy::cast_precision_loss)] // корпус — сотни документов
        let avg_len = if doc_lens.is_empty() {
            0.0
        } else {
            doc_lens.iter().sum::<u32>() as f32 / doc_lens.len() as f32
        };
        Self {
            postings,
            doc_lens,
            avg_len,
        }
    }

    /// Топ-`limit` документов по убыванию скора. Пустой результат — ни один терм не найден.
    #[must_use]
    pub fn query(&self, query: &str, limit: usize) -> Vec<(u32, f32)> {
        let n_docs = self.doc_lens.len();
        if n_docs == 0 {
            return Vec::new();
        }
        let mut scores: HashMap<u32, f32> = HashMap::new();
        for term in tokenize(query) {
            let Some(posting) = self.postings.get(&term) else {
                continue;
            };
            #[allow(clippy::cast_precision_loss)] // корпус — сотни документов
            let idf = (((n_docs as f32 - posting.len() as f32 + 0.5)
                / (posting.len() as f32 + 0.5))
                + 1.0)
                .ln();
            for &(doc, freq) in posting {
                #[allow(clippy::cast_precision_loss)]
                let (freq, dl) = (freq as f32, self.doc_lens[doc as usize] as f32);
                let score =
                    idf * (freq * (K1 + 1.0)) / (freq + K1 * (1.0 - B + B * dl / self.avg_len));
                *scores.entry(doc).or_insert(0.0) += score;
            }
        }
        let mut ranked: Vec<(u32, f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(limit);
        ranked
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn identifier_splitting() {
        assert_eq!(tokenize("SearchDashboards"), ["search", "dashboards"]);
        assert_eq!(tokenize("grafana-ai_v2"), ["grafana", "ai", "v2"]);
        assert_eq!(
            tokenize("github__create_issue"),
            ["github", "create", "issue"]
        );
    }

    #[test]
    fn ranks_relevant_doc_first() {
        let idx = Bm25Index::build([
            "github create_issue — open a new issue in a repository",
            "linear list_projects — enumerate projects",
            "github merge_pull_request — merge a PR",
        ]);
        let top = idx.query("create issue", 2);
        assert_eq!(top[0].0, 0);
        let top = idx.query("merge PR", 2);
        assert_eq!(top[0].0, 2);
        assert!(idx.query("nonexistent_term_zzz", 5).is_empty());
    }

    #[test]
    fn empty_corpus() {
        assert!(Bm25Index::build([]).query("x", 5).is_empty());
    }
}
