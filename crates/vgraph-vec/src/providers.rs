//! 임베더 공급자 — spec 문자열로 선택 (설치형·외부 API 모두).
//!
//! | spec | 공급자 | 비고 |
//! |---|---|---|
//! | `toy` / `toy:<dim>` | 내장 HashBag | 결정적 — 골든·데모 전용 |
//! | `openai:<model>` | OpenAI embeddings API | env `OPENAI_API_KEY` (+`OPENAI_BASE_URL`) |
//! | `ollama:<model>[@<base-url>]` | 로컬 Ollama 서버 | "같이 설치" 경로 — 기본 localhost:11434 |
//! | `http:<url>` | 자가 호스팅 규약 | POST {"texts":[…]} → {"vectors":[[…]]} |
//!
//! model_id = spec 기반 — 모델이 다르면 벡터 네임스페이스가 분리되어 병존한다.

use crate::{EmbedError, Embedder, HashBagEmbedder};

pub fn embedder_from_spec(spec: &str) -> Result<Box<dyn Embedder>, EmbedError> {
    if spec == "toy" {
        return Ok(Box::new(HashBagEmbedder::default()));
    }
    if let Some(dim) = spec.strip_prefix("toy:") {
        let dim: usize = dim
            .parse()
            .map_err(|_| EmbedError(format!("bad toy dim: {spec}")))?;
        return Ok(Box::new(HashBagEmbedder { dim }));
    }
    if let Some(model) = spec.strip_prefix("openai:") {
        if model.is_empty() {
            return Err(EmbedError("openai:<model> 형식".into()));
        }
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| EmbedError("OPENAI_API_KEY 환경변수 필요".into()))?;
        let base =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        return Ok(Box::new(OpenAiEmbedder {
            model: model.into(),
            api_key,
            base,
        }));
    }
    if let Some(rest) = spec.strip_prefix("ollama:") {
        if rest.is_empty() {
            return Err(EmbedError("ollama:<model>[@url] 형식".into()));
        }
        let (model, url) = match rest.split_once('@') {
            Some((m, u)) => (m.to_string(), u.to_string()),
            None => (rest.to_string(), "http://localhost:11434".to_string()),
        };
        return Ok(Box::new(OllamaEmbedder { model, url }));
    }
    if spec.starts_with("https://") {
        return Ok(Box::new(HttpEmbedder {
            url: spec.to_string(),
        }));
    }
    if let Some(url) = spec.strip_prefix("http:") {
        // "http:http://host/embed" 또는 "http://host/embed" 축약 모두 허용
        let url = if url.starts_with("//") {
            format!("http:{url}")
        } else {
            url.to_string()
        };
        return Ok(Box::new(HttpEmbedder { url }));
    }
    Err(EmbedError(format!(
        "알 수 없는 임베더 spec: {spec} (toy | toy:<dim> | openai:<model> | ollama:<model>[@url] | http:<url>)"
    )))
}

// ---------- OpenAI (외부 API) ----------

pub struct OpenAiEmbedder {
    model: String,
    api_key: String,
    base: String,
}

impl Embedder for OpenAiEmbedder {
    fn model_id(&self) -> String {
        format!("openai:{}", self.model)
    }
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let resp: serde_json::Value = ureq::post(&format!("{}/embeddings", self.base))
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .send_json(serde_json::json!({ "model": self.model, "input": texts }))
            .map_err(|e| EmbedError(format!("openai: {e}")))?
            .into_json()
            .map_err(|e| EmbedError(format!("openai json: {e}")))?;
        let data = resp["data"]
            .as_array()
            .ok_or_else(|| EmbedError("openai: no data".into()))?;
        data.iter()
            .map(|d| {
                d["embedding"]
                    .as_array()
                    .ok_or_else(|| EmbedError("openai: no embedding".into()))
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_f64())
                            .map(|f| f as f32)
                            .collect()
                    })
            })
            .collect()
    }
}

// ---------- Ollama (로컬 설치형) ----------

pub struct OllamaEmbedder {
    model: String,
    url: String,
}

impl Embedder for OllamaEmbedder {
    fn model_id(&self) -> String {
        format!("ollama:{}", self.model)
    }
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        // /api/embed = 배치 지원 (Ollama ≥0.3). 구버전은 /api/embeddings 단건 — 미지원 시 오류로 안내.
        let resp: serde_json::Value = ureq::post(&format!("{}/api/embed", self.url))
            .send_json(serde_json::json!({ "model": self.model, "input": texts }))
            .map_err(|e| {
                EmbedError(format!(
                    "ollama({}): {e} — ollama 실행·모델 pull 확인",
                    self.url
                ))
            })?
            .into_json()
            .map_err(|e| EmbedError(format!("ollama json: {e}")))?;
        let embs = resp["embeddings"].as_array().ok_or_else(|| {
            EmbedError("ollama: embeddings 없음 (구버전이면 업데이트 필요)".into())
        })?;
        Ok(embs
            .iter()
            .map(|a| {
                a.as_array()
                    .map(|xs| {
                        xs.iter()
                            .filter_map(|x| x.as_f64())
                            .map(|f| f as f32)
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect())
    }
}

// ---------- 자가 호스팅 규약 (generic) ----------

/// POST <url> {"texts": […]} → {"vectors": [[…], …]} — TEI/vLLM 등 앞단 셔틀로 감싸 사용.
pub struct HttpEmbedder {
    url: String,
}

impl Embedder for HttpEmbedder {
    fn model_id(&self) -> String {
        format!("http:{}", self.url)
    }
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let resp: serde_json::Value = ureq::post(&self.url)
            .send_json(serde_json::json!({ "texts": texts }))
            .map_err(|e| EmbedError(format!("http embedder: {e}")))?
            .into_json()
            .map_err(|e| EmbedError(format!("http embedder json: {e}")))?;
        let vecs = resp["vectors"]
            .as_array()
            .ok_or_else(|| EmbedError("http embedder: vectors 없음".into()))?;
        Ok(vecs
            .iter()
            .map(|a| {
                a.as_array()
                    .map(|xs| {
                        xs.iter()
                            .filter_map(|x| x.as_f64())
                            .map(|f| f as f32)
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parsing() {
        assert_eq!(embedder_from_spec("toy").unwrap().model_id(), "toy-32");
        assert_eq!(embedder_from_spec("toy:64").unwrap().model_id(), "toy-64");
        assert_eq!(
            embedder_from_spec("ollama:nomic-embed-text")
                .unwrap()
                .model_id(),
            "ollama:nomic-embed-text"
        );
        assert!(embedder_from_spec("openai:").is_err());
        assert!(embedder_from_spec("unknown:x").is_err());
        // openai는 키 없으면 즉시 오류 (지연 실패 금지)
        std::env::remove_var("OPENAI_API_KEY");
        assert!(embedder_from_spec("openai:text-embedding-3-small").is_err());
    }
}
