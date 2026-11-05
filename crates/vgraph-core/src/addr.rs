//! 주소 문법 (설계 §3.1, lmd §5 계승):
//! `graph://<ns>/<slug>[@<branch>][.<commit>][#<node>]`
//!
//! ns/slug 문자셋: [A-Za-z0-9_-]. branch 는 거기에 구분용 `/` 가 더해진다(`valid_branch_name`). commit = "sha256:<hex64>" 또는 hex64.
//! node = slug 또는 26자 ULID.

use crate::canon::Hash256;
use crate::id::Uuid;
use smol_str::SmolStr;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeSel {
    Slug(SmolStr),
    Uuid(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphAddr {
    pub ns: SmolStr,
    pub slug: SmolStr,
    pub branch: Option<SmolStr>,
    pub commit: Option<Hash256>,
    pub node: Option<NodeSel>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum AddrError {
    #[error("missing graph:// scheme")]
    Scheme,
    #[error("empty segment: {0}")]
    Empty(&'static str),
    #[error("invalid char in {0}")]
    BadChar(&'static str),
    #[error(
        "branch 이름은 영문·숫자·-·_ 와 구분용 / 만 쓸 수 있고, 비어 있거나 / 로 시작·끝나거나 // 가 들어 있으면 안 됩니다"
    )]
    BadBranch,
    #[error("invalid commit hash")]
    BadCommit,
}

fn valid_seg(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 브랜치 이름 규칙 — 브랜치를 새로 만들거나 주소의 `@` 뒤를 읽는 모든 곳이 이 함수 하나를 쓴다.
///
/// 영문·숫자·`-`·`_`와 구분용 `/`만 허용한다. 비어 있거나 `/`로 시작·끝나거나 `//`가 들어 있으면 거부한다.
/// `.`·`@`·`#`·공백은 주소 문법(커밋 핀·브랜치·노드 선택)과 부딪히므로 허용하지 않는다.
/// 길이 상한은 두지 않는다 — 저장소 계층(ref 문자열)에 상한이 없고, 이번 규칙은 기존 이름을 좁히지 않는 것이 목적이다.
/// 이름 인자로 쓰는 기존 경로(목록·병합)는 이 검사를 하지 않으므로, 옛 규칙 밖 이름의 ref도 계속 다룰 수 있다.
pub fn valid_branch_name(name: &str) -> Result<(), AddrError> {
    let chars_ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/'));
    let shape_ok =
        !name.is_empty() && !name.starts_with('/') && !name.ends_with('/') && !name.contains("//");
    if chars_ok && shape_ok {
        Ok(())
    } else {
        Err(AddrError::BadBranch)
    }
}

impl FromStr for GraphAddr {
    type Err = AddrError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let rest = input.strip_prefix("graph://").ok_or(AddrError::Scheme)?;

        // #node 분리 (뒤에서부터)
        let (rest, node) = match rest.split_once('#') {
            Some((r, n)) => {
                if n.is_empty() {
                    return Err(AddrError::Empty("node"));
                }
                let sel = if n.len() == 26 {
                    n.parse::<Uuid>()
                        .map(NodeSel::Uuid)
                        .unwrap_or_else(|_| NodeSel::Slug(SmolStr::new(n)))
                } else {
                    NodeSel::Slug(SmolStr::new(n))
                };
                (r, Some(sel))
            }
            None => (rest, None),
        };

        // ns/slug[@branch][.commit]
        let (ns, tail) = rest.split_once('/').ok_or(AddrError::Empty("slug"))?;
        if !valid_seg(ns) {
            return Err(AddrError::BadChar("ns"));
        }

        // slug는 '@' 또는 '.' 전까지
        let (slug, after) = match tail.find(['@', '.']) {
            Some(i) => (&tail[..i], &tail[i..]),
            None => (tail, ""),
        };
        if !valid_seg(slug) {
            return Err(AddrError::BadChar("slug"));
        }

        let (branch, commit_s) = if let Some(b) = after.strip_prefix('@') {
            match b.split_once('.') {
                Some((br, c)) => (Some(br), Some(c)),
                None => (Some(b), None),
            }
        } else if let Some(c) = after.strip_prefix('.') {
            (None, Some(c))
        } else {
            (None, None)
        };

        let branch = match branch {
            Some(b) => {
                valid_branch_name(b)?;
                Some(SmolStr::new(b))
            }
            None => None,
        };
        let commit = match commit_s {
            Some(c) => Some(c.parse::<Hash256>().map_err(|_| AddrError::BadCommit)?),
            None => None,
        };

        Ok(GraphAddr {
            ns: SmolStr::new(ns),
            slug: SmolStr::new(slug),
            branch,
            commit,
            node,
        })
    }
}

impl fmt::Display for GraphAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "graph://{}/{}", self.ns, self.slug)?;
        if let Some(b) = &self.branch {
            write!(f, "@{b}")?;
        }
        if let Some(c) = &self.commit {
            write!(f, ".{c}")?;
        }
        match &self.node {
            Some(NodeSel::Slug(s)) => write!(f, "#{s}")?,
            Some(NodeSel::Uuid(u)) => write!(f, "#{u}")?,
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canon::sha256;

    #[test]
    fn parse_basic() {
        let a: GraphAddr = "graph://acme/bookclub".parse().unwrap();
        assert_eq!(a.ns, "acme");
        assert_eq!(a.slug, "bookclub");
        assert_eq!(a.branch, None);
        assert_eq!(a.commit, None);
        assert_eq!(a.node, None);
    }

    #[test]
    fn parse_full() {
        let h = sha256(b"c0");
        let s = format!("graph://acme/bookclub@main.{h}#cap-auth");
        let a: GraphAddr = s.parse().unwrap();
        assert_eq!(a.branch.as_deref(), Some("main"));
        assert_eq!(a.commit, Some(h));
        assert_eq!(a.node, Some(NodeSel::Slug("cap-auth".into())));
        assert_eq!(a.to_string(), s); // roundtrip
    }

    #[test]
    fn parse_branch_only_and_pin_only() {
        let a: GraphAddr = "graph://acme/bookclub@dev-b".parse().unwrap();
        assert_eq!(a.branch.as_deref(), Some("dev-b"));
        let h = sha256(b"x");
        let b: GraphAddr = format!("graph://acme/bookclub.{h}").parse().unwrap();
        assert_eq!(b.branch, None);
        assert_eq!(b.commit, Some(h));
    }

    #[test]
    fn parse_node_uuid() {
        let a: GraphAddr = "graph://acme/bookclub#00000000000000000000000003"
            .parse()
            .unwrap();
        assert!(matches!(a.node, Some(NodeSel::Uuid(_))));
    }

    #[test]
    fn parse_errors() {
        assert_eq!("acme/bookclub".parse::<GraphAddr>(), Err(AddrError::Scheme));
        assert!("graph://acme".parse::<GraphAddr>().is_err());
        assert!("graph://acme/book club".parse::<GraphAddr>().is_err());
        assert!("graph://acme/bookclub@main.zzz"
            .parse::<GraphAddr>()
            .is_err());
    }
}
