//! Structural Search Replace
//!
//! Allows searching the AST for code that matches one or more patterns and then replacing that code
//! based on a template.

mod matching;
mod parsing;
mod replacing;
mod search;
#[macro_use]
mod errors;
#[cfg(test)]
mod tests;

pub use crate::errors::SsrError;
pub use crate::matching::Match;
use crate::matching::MatchFailureReason;
use hir::Semantics;
use ra_db::{FileId, FileRange};
use ra_syntax::{ast, AstNode, SyntaxNode, TextRange};
use ra_text_edit::TextEdit;

// A structured search replace rule. Create by calling `parse` on a str.
#[derive(Debug)]
pub struct SsrRule {
    /// A structured pattern that we're searching for.
    pattern: parsing::RawPattern,
    /// What we'll replace it with.
    template: parsing::RawPattern,
    parsed_rules: Vec<parsing::ParsedRule>,
}

#[derive(Debug)]
pub struct SsrPattern {
    raw: parsing::RawPattern,
    parsed_rules: Vec<parsing::ParsedRule>,
}

#[derive(Debug, Default)]
pub struct SsrMatches {
    pub matches: Vec<Match>,
}

/// Searches a crate for pattern matches and possibly replaces them with something else.
pub struct MatchFinder<'db> {
    /// Our source of information about the user's code.
    sema: Semantics<'db, ra_ide_db::RootDatabase>,
    rules: Vec<parsing::ParsedRule>,
}

impl<'db> MatchFinder<'db> {
    pub fn new(db: &'db ra_ide_db::RootDatabase) -> MatchFinder<'db> {
        MatchFinder { sema: Semantics::new(db), rules: Vec::new() }
    }

    /// Adds a rule to be applied. The order in which rules are added matters. Earlier rules take
    /// precedence. If a node is matched by an earlier rule, then later rules won't be permitted to
    /// match to it.
    pub fn add_rule(&mut self, rule: SsrRule) {
        self.add_parsed_rules(rule.parsed_rules);
    }

    /// Adds a search pattern. For use if you intend to only call `find_matches_in_file`. If you
    /// intend to do replacement, use `add_rule` instead.
    pub fn add_search_pattern(&mut self, pattern: SsrPattern) {
        self.add_parsed_rules(pattern.parsed_rules);
    }

    pub fn edits_for_file(&self, file_id: FileId) -> Option<TextEdit> {
        let matches = self.find_matches_in_file(file_id);
        if matches.matches.is_empty() {
            None
        } else {
            use ra_db::SourceDatabaseExt;
            Some(replacing::matches_to_edit(
                &matches,
                &self.sema.db.file_text(file_id),
                &self.rules,
            ))
        }
    }

    pub fn find_matches_in_file(&self, file_id: FileId) -> SsrMatches {
        let file = self.sema.parse(file_id);
        let code = file.syntax();
        let mut matches = SsrMatches::default();
        self.slow_scan_node(code, &None, &mut matches.matches);
        matches
    }

    /// Finds all nodes in `file_id` whose text is exactly equal to `snippet` and attempts to match
    /// them, while recording reasons why they don't match. This API is useful for command
    /// line-based debugging where providing a range is difficult.
    pub fn debug_where_text_equal(&self, file_id: FileId, snippet: &str) -> Vec<MatchDebugInfo> {
        use ra_db::SourceDatabaseExt;
        let file = self.sema.parse(file_id);
        let mut res = Vec::new();
        let file_text = self.sema.db.file_text(file_id);
        let mut remaining_text = file_text.as_str();
        let mut base = 0;
        let len = snippet.len() as u32;
        while let Some(offset) = remaining_text.find(snippet) {
            let start = base + offset as u32;
            let end = start + len;
            self.output_debug_for_nodes_at_range(
                file.syntax(),
                FileRange { file_id, range: TextRange::new(start.into(), end.into()) },
                &None,
                &mut res,
            );
            remaining_text = &remaining_text[offset + snippet.len()..];
            base = end;
        }
        res
    }

    fn add_parsed_rules(&mut self, parsed_rules: Vec<parsing::ParsedRule>) {
        for mut parsed_rule in parsed_rules {
            parsed_rule.index = self.rules.len();
            self.rules.push(parsed_rule);
        }
    }

    fn output_debug_for_nodes_at_range(
        &self,
        node: &SyntaxNode,
        range: FileRange,
        restrict_range: &Option<FileRange>,
        out: &mut Vec<MatchDebugInfo>,
    ) {
        for node in node.children() {
            let node_range = self.sema.original_range(&node);
            if node_range.file_id != range.file_id || !node_range.range.contains_range(range.range)
            {
                continue;
            }
            if node_range.range == range.range {
                for rule in &self.rules {
                    // For now we ignore rules that have a different kind than our node, otherwise
                    // we get lots of noise. If at some point we add support for restricting rules
                    // to a particular kind of thing (e.g. only match type references), then we can
                    // relax this.
                    if rule.pattern.kind() != node.kind() {
                        continue;
                    }
                    out.push(MatchDebugInfo {
                        matched: matching::get_match(true, rule, &node, restrict_range, &self.sema)
                            .map_err(|e| MatchFailureReason {
                                reason: e.reason.unwrap_or_else(|| {
                                    "Match failed, but no reason was given".to_owned()
                                }),
                            }),
                        pattern: rule.pattern.clone(),
                        node: node.clone(),
                    });
                }
            } else if let Some(macro_call) = ast::MacroCall::cast(node.clone()) {
                if let Some(expanded) = self.sema.expand(&macro_call) {
                    if let Some(tt) = macro_call.token_tree() {
                        self.output_debug_for_nodes_at_range(
                            &expanded,
                            range,
                            &Some(self.sema.original_range(tt.syntax())),
                            out,
                        );
                    }
                }
            }
            self.output_debug_for_nodes_at_range(&node, range, restrict_range, out);
        }
    }
}

pub struct MatchDebugInfo {
    node: SyntaxNode,
    /// Our search pattern parsed as an expression or item, etc
    pattern: SyntaxNode,
    matched: Result<Match, MatchFailureReason>,
}

impl std::fmt::Debug for MatchDebugInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.matched {
            Ok(_) => writeln!(f, "Node matched")?,
            Err(reason) => writeln!(f, "Node failed to match because: {}", reason.reason)?,
        }
        writeln!(
            f,
            "============ AST ===========\n\
            {:#?}",
            self.node
        )?;
        writeln!(f, "========= PATTERN ==========")?;
        writeln!(f, "{:#?}", self.pattern)?;
        writeln!(f, "============================")?;
        Ok(())
    }
}

impl SsrMatches {
    /// Returns `self` with any nested matches removed and made into top-level matches.
    pub fn flattened(self) -> SsrMatches {
        let mut out = SsrMatches::default();
        self.flatten_into(&mut out);
        out
    }

    fn flatten_into(self, out: &mut SsrMatches) {
        for mut m in self.matches {
            for p in m.placeholder_values.values_mut() {
                std::mem::replace(&mut p.inner_matches, SsrMatches::default()).flatten_into(out);
            }
            out.matches.push(m);
        }
    }
}

impl Match {
    pub fn matched_text(&self) -> String {
        self.matched_node.text().to_string()
    }
}

impl std::error::Error for SsrError {}

#[cfg(test)]
impl MatchDebugInfo {
    pub(crate) fn match_failure_reason(&self) -> Option<&str> {
        self.matched.as_ref().err().map(|r| r.reason.as_str())
    }
}
