use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use rusqlite::params;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{COLUMNS, Entry, Error, Memory, Result, entry};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// Describe the problem to solve, including error text or code identifiers.
    pub task: String,
    pub context: Option<String>,
    /// An unverified possible solution. Alternatives are still retrieved.
    pub hypothesis: Option<String>,
    /// all (default), note, wiki, or message.
    pub kind: Option<String>,
    pub limit: Option<usize>,
    /// Default false: current branch and knowledge delivered to it. Other branches remain labelled.
    pub all_branches: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub mode: String,
    pub notice: Option<String>,
    pub results: Vec<Value>,
}

#[derive(Debug)]
struct Candidate {
    item: Entry,
    vector: Option<Vec<f32>>,
}

impl Memory {
    fn embedding_key(&self) -> String {
        format!(
            "v1:{}:{}",
            self.config.embedding_url, self.config.embedding_model
        )
    }

    pub(crate) async fn embed(
        &self,
        texts: &[String],
    ) -> std::result::Result<Vec<Vec<f32>>, String> {
        let url = reqwest::Url::parse(&self.config.embedding_url).map_err(|e| e.to_string())?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err("embedding URL must not contain credentials".into());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_secs(90))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let response=client.post(url).json(&json!({"model":self.config.embedding_model,"input":texts,"truncate":true}))
            .send().await.map_err(|_|"Local embedding service unavailable. Start Ollama and run `ollama pull embeddinggemma`, or configure [memory].".to_owned())?;
        if !response.status().is_success() {
            return Err(format!(
                "Embedding service returned HTTP {}; check [memory].embedding_model and the local model installation",
                response.status()
            ));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| "Invalid embedding response".to_owned())?;
        let mut vectors: Vec<Vec<f32>> = serde_json::from_value(value["embeddings"].clone())
            .map_err(|_| "Missing embeddings in response".to_owned())?;
        if vectors.len() != texts.len() {
            return Err("Embedding count does not match input count".into());
        }
        let dim = vectors.first().map_or(0, Vec::len);
        if dim == 0
            || dim > 8192
            || vectors
                .iter()
                .any(|v| v.len() != dim || v.iter().any(|x| !x.is_finite()))
        {
            return Err("Invalid embedding dimensions or non-finite values".into());
        }
        for vector in &mut vectors {
            let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
            if !norm.is_finite() || norm <= f32::EPSILON {
                return Err("Invalid zero embedding".into());
            }
            for value in vector {
                *value /= norm;
            }
        }
        Ok(vectors)
    }

    fn candidates(&self, kind: &str, all_branches: bool) -> Result<Vec<Candidate>> {
        let connection = self.connect()?;
        let mut query=connection.prepare(&format!("SELECT {COLUMNS},embedding,embedding_model FROM entries e WHERE
            (?1='all' OR kind=?1) AND (?2 OR branch=?3 OR EXISTS(
                SELECT 1 FROM evidence WHERE entry=e.id AND revision=e.revision AND kind='merged' AND target=?4))"))?;
        let model = self.embedding_key();
        Ok(query
            .query_map(
                params![
                    kind,
                    all_branches,
                    self.branch,
                    format!("refs/heads/{}", self.branch)
                ],
                |r| {
                    let bytes: Option<Vec<u8>> = r.get(12)?;
                    let stored: Option<String> = r.get(13)?;
                    Ok(Candidate {
                        item: entry(r)?,
                        vector: bytes
                            .filter(|_| stored.as_deref() == Some(&model))
                            .and_then(|b| decode(&b)),
                    })
                },
            )?
            .collect::<std::result::Result<_, _>>()?)
    }

    /// Cheap pre-search the harness runs for every prompt: keyword matches among wiki
    /// nodes and notes, one line each. No embedding call and no Git commands.
    pub fn hints(&self, text: &str) -> Result<String> {
        // Short words are mostly stop words and would match every document.
        let terms: Vec<_> = text
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| s.chars().count() >= 4)
            .take(32)
            .map(|s| format!("\"{s}\""))
            .collect();
        if terms.is_empty() {
            return Ok(String::new());
        }
        let connection = self.connect()?;
        let mut query = connection.prepare("SELECT e.kind,e.id,e.title,substr(e.body,1,300) FROM entries_fts JOIN entries e ON e.id=entries_fts.rowid
            WHERE entries_fts MATCH ?1 AND e.kind IN ('wiki','note')
            -- a vault that is only seeded knows nothing: its map, link grammar and stubs are not hints
            AND e.body NOT LIKE '[schema]%' AND e.body NOT LIKE '[index]%' AND instr(e.body,?4)=0
            AND (e.branch=?2 OR EXISTS(
                SELECT 1 FROM evidence WHERE entry=e.id AND revision=e.revision AND kind='merged' AND target=?3))
            ORDER BY bm25(entries_fts,5.0,1.0) LIMIT 3")?;
        let rows = query.query_map(
            params![
                terms.join(" OR "),
                self.branch,
                format!("refs/heads/{}", self.branch),
                makedo_wiki::seed::SEED_MARKER
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?;
        let mut out = String::new();
        for row in rows {
            let (kind, id, title, summary) = row?;
            let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
            let _ = writeln!(out, "- {kind} #{id} {title} — {summary}");
        }
        Ok(out)
    }

    fn lexical(&self, text: &str, kind: &str, all_branches: bool) -> Result<Vec<i64>> {
        let terms: Vec<_> = text
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| !s.is_empty())
            .take(64)
            .map(|s| format!("\"{s}\""))
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let connection = self.connect()?;
        let mut query=connection.prepare("SELECT e.id FROM entries_fts JOIN entries e ON e.id=entries_fts.rowid WHERE entries_fts MATCH ?1
            AND (?2='all' OR e.kind=?2) AND (?3 OR e.branch=?4 OR EXISTS(
                SELECT 1 FROM evidence WHERE entry=e.id AND revision=e.revision AND kind='merged' AND target=?5))
            ORDER BY bm25(entries_fts,5.0,1.0) LIMIT 40")?;
        Ok(query
            .query_map(
                params![
                    terms.join(" OR "),
                    kind,
                    all_branches,
                    self.branch,
                    format!("refs/heads/{}", self.branch)
                ],
                |r| r.get(0),
            )?
            .collect::<std::result::Result<_, _>>()?)
    }

    /// Exact cosine scan is deliberately small and portable. Add ANN only when measured corpus size warrants it.
    #[allow(clippy::too_many_lines)] // Fetch, infer outside transactions, cache, then rank.
    pub async fn search(&self, args: SearchArgs) -> Result<SearchResult> {
        if args.task.trim().is_empty() {
            return Err(Error::Invalid("task must not be empty".into()));
        }
        let kind = args.kind.as_deref().unwrap_or("all");
        if !["all", "note", "wiki", "message"].contains(&kind) {
            return Err(Error::Invalid(
                "kind must be all, note, wiki or message".into(),
            ));
        }
        let query: String = format!(
            "{}\n{}\n{}",
            args.task,
            args.context.unwrap_or_default(),
            args.hypothesis.unwrap_or_default()
        )
        .chars()
        .take(8000)
        .collect();
        let memory = self.clone();
        let kind = kind.to_owned();
        let query_copy = query.clone();
        let (mut candidates, lexical) = tokio::task::spawn_blocking(move || {
            memory.refresh_delivery()?;
            Ok::<_, Error>((
                memory.candidates(&kind, args.all_branches.unwrap_or(false))?,
                memory.lexical(&query_copy, &kind, args.all_branches.unwrap_or(false))?,
            ))
        })
        .await
        .map_err(|e| Error::Invalid(e.to_string()))??;
        let mut notice = None;
        let mut query_vector = None;
        if !candidates.is_empty() {
            match self.embed(&[query]).await {
                Ok(mut v) => query_vector = v.pop(),
                Err(error) => notice = Some(error),
            }
        }
        if let Some(vector) = &query_vector {
            // Populate missing vectors on demand, without a transaction across inference.
            let missing: Vec<usize> = candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| c.vector.as_ref().is_none_or(|v| v.len() != vector.len()))
                .map(|(i, _)| i)
                .collect();
            for indices in missing.chunks(16) {
                let texts: Vec<_> = indices
                    .iter()
                    .map(|i| {
                        format!(
                            "{}\n{}",
                            candidates[*i].item.title, candidates[*i].item.text
                        )
                    })
                    .collect();
                match self.embed(&texts).await {
                    Ok(vectors) if vectors.iter().all(|v| v.len() == vector.len()) => {
                        let updates: Vec<_> = indices
                            .iter()
                            .zip(vectors)
                            .map(|(i, v)| {
                                candidates[*i].vector = Some(v.clone());
                                (
                                    candidates[*i].item.id,
                                    candidates[*i].item.revision,
                                    encode(&v),
                                )
                            })
                            .collect();
                        let memory = self.clone();
                        tokio::task::spawn_blocking(move||{
                            let mut connection=memory.connect()?;
                            let tx=connection.transaction()?;
                            for (id,revision,bytes) in updates {
                                tx.execute("UPDATE entries SET embedding=?1,embedding_model=?2 WHERE id=?3 AND revision=?4",params![bytes,memory.embedding_key(),id,revision])?;
                            }
                            tx.commit()?;
                            Ok::<_,Error>(())
                        }).await.map_err(|e|Error::Invalid(e.to_string()))??;
                    }
                    Ok(_) => {
                        notice=Some("Embedding dimensions changed during indexing; retry with a stable model".into());
                        break;
                    }
                    Err(error) => {
                        notice = Some(error);
                        break;
                    }
                }
            }
        }
        let memory = self.clone();
        tokio::task::spawn_blocking(move || {
            memory.rank(
                candidates,
                &lexical,
                query_vector.as_deref(),
                notice,
                args.limit.unwrap_or(8).clamp(1, 25),
            )
        })
        .await
        .map_err(|e| Error::Invalid(e.to_string()))?
    }

    fn rank(
        &self,
        candidates: Vec<Candidate>,
        lexical: &[i64],
        query: Option<&[f32]>,
        notice: Option<String>,
        limit: usize,
    ) -> Result<SearchResult> {
        let mut semantic: Vec<(i64, f32)> = candidates
            .iter()
            .filter_map(|c| {
                let (q, v) = (query?, c.vector.as_ref()?);
                (q.len() == v.len())
                    .then(|| (c.item.id, q.iter().zip(v).map(|(a, b)| a * b).sum::<f32>()))
            })
            .collect();
        semantic.sort_by(|a, b| b.1.total_cmp(&a.1));
        let has_semantic = !semantic.is_empty();
        let mut scores: BTreeMap<i64, (f64, Option<f32>)> = BTreeMap::new();
        let available: std::collections::BTreeSet<_> =
            candidates.iter().map(|c| c.item.id).collect();
        for (rank, id) in lexical
            .iter()
            .filter(|id| available.contains(id))
            .take(40)
            .enumerate()
        {
            scores.entry(*id).or_default().0 +=
                1.0 / (61.0 + f64::from(u32::try_from(rank).unwrap_or(u32::MAX)));
        }
        for (rank, (id, similarity)) in semantic.iter().take(40).enumerate() {
            let score = scores.entry(*id).or_default();
            score.0 += 1.0 / (61.0 + f64::from(u32::try_from(rank).unwrap_or(u32::MAX)));
            score.1 = Some(*similarity);
        }
        let mut results = Vec::new();
        let connection = self.connect()?;
        let mut replacements =
            connection.prepare("SELECT id FROM entries WHERE supersedes=?1 ORDER BY id")?;
        for candidate in candidates {
            let item = candidate.item;
            let Some((relevance, similarity)) = scores.get(&item.id) else {
                continue;
            };
            let evidence = self.evidence(item.id)?;
            let current: Vec<_> = evidence
                .iter()
                .filter(|e| e.revision == item.revision)
                .collect();
            let mut checks = BTreeMap::new();
            for evidence in &current {
                if matches!(evidence.kind.as_str(), "check_passed" | "check_failed") {
                    let detail: Value = serde_json::from_str(&evidence.detail)?;
                    checks.insert(
                        detail["command"].as_str().unwrap_or("").to_owned(),
                        evidence.kind == "check_passed",
                    );
                }
            }
            let superseded_by: Vec<i64> = replacements
                .query_map([item.id], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            let negative = current
                .iter()
                .any(|e| matches!(e.kind.as_str(), "reverted" | "unreachable"))
                || checks.values().any(|passed| !passed)
                || !superseded_by.is_empty();
            let adjustment =
                if negative || matches!(item.assessment.as_str(), "rejected" | "disputed") {
                    0.9
                } else if current.iter().any(|e| e.kind == "merged") {
                    1.03
                } else {
                    1.0
                };
            results.push(json!({"id":item.id,"uid":item.uid,"revision":item.revision,"kind":item.kind,"title":item.title,
                "summary":item.text.chars().take(500).collect::<String>(),"branch":item.branch,"source":item.source,
                "relevance_score":relevance,"ranking_score":relevance*adjustment,"semantic_similarity":similarity,
                "assessment":item.assessment,"evidence":evidence,"supersedes":item.supersedes,"superseded_by":superseded_by}));
        }
        results.sort_by(|a, b| {
            b["ranking_score"]
                .as_f64()
                .unwrap_or_default()
                .total_cmp(&a["ranking_score"].as_f64().unwrap_or_default())
        });
        let mut seen = std::collections::BTreeSet::new();
        results.retain(|r| seen.insert(r["summary"].as_str().unwrap_or("").to_owned()));
        results.truncate(limit);
        for (i, result) in results.iter_mut().enumerate() {
            result["rank"] = json!(i + 1);
        }
        Ok(SearchResult {
            mode: if has_semantic {
                "hybrid"
            } else {
                "lexical_only"
            }
            .into(),
            notice,
            results,
        })
    }

    pub(crate) fn save_vector(&self, id: i64, revision: i64, vector: &[f32]) -> Result<()> {
        self.connect()?.execute(
            "UPDATE entries SET embedding=?1,embedding_model=?2 WHERE id=?3 AND revision=?4",
            params![encode(vector), self.embedding_key(), id, revision],
        )?;
        Ok(())
    }
}

fn encode(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|n| n.to_le_bytes()).collect()
}
fn decode(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    bytes
        .chunks_exact(4)
        .map(|b| {
            let n = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            n.is_finite().then_some(n)
        })
        .collect()
}
