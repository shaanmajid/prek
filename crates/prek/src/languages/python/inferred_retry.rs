use std::sync::LazyLock;

use regex::Regex;

use crate::languages::python::PythonRequest;
use crate::languages::version::LanguageRequest;
use crate::process;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum VersionPrecision {
    Major,
    MajorMinor,
    MajorMinorPatch,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct InferredLowerBound {
    version: semver::Version,
    inclusive: bool,
    precision: VersionPrecision,
}

impl InferredLowerBound {
    fn operator(&self) -> &'static str {
        if self.inclusive { ">=" } else { ">" }
    }

    fn is_stricter_than(&self, other: &Self) -> bool {
        match self.version.cmp(&other.version) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Less => false,
            std::cmp::Ordering::Equal => !self.inclusive && other.inclusive,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct InferredUpperBound {
    version: semver::Version,
    inclusive: bool,
    precision: VersionPrecision,
}

impl InferredUpperBound {
    fn operator(&self) -> &'static str {
        if self.inclusive { "<=" } else { "<" }
    }

    fn is_stricter_than(&self, other: &Self) -> bool {
        match self.version.cmp(&other.version) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => !self.inclusive && other.inclusive,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(super) struct InferredRetryConstraint {
    request: String,
    requirement: semver::VersionReq,
    candidate: semver::Version,
    lower: InferredLowerBound,
    upper: InferredUpperBound,
}

pub(super) fn infer_retry_constraint_from_error(
    error: &process::Error,
) -> Option<InferredRetryConstraint> {
    let process::Error::Status {
        error: process::StatusError {
            output: Some(output),
            ..
        },
        ..
    } = error
    else {
        return None;
    };

    infer_retry_constraint(&String::from_utf8_lossy(&output.stderr))
}

fn infer_retry_constraint(stderr: &str) -> Option<InferredRetryConstraint> {
    static PYTHON_BOUND: LazyLock<Regex> = LazyLock::new(|| {
        // uv does not expose structured resolver diagnostics for this case yet,
        // so retry inference intentionally parses the narrow human-readable
        // explanation and fails closed if that wording changes.
        Regex::new(r"does not satisfy\s+Python\s*(>=|>)\s*([0-9]+(?:\.[0-9]+){0,2})")
            .expect("inferred Python bound regex must be valid")
    });

    PYTHON_BOUND
        .captures_iter(stderr)
        .filter_map(|captures| {
            let op = captures.get(1)?.as_str();
            let version = captures.get(2)?.as_str();
            parse_inferred_lower_bound(op, version)
        })
        .reduce(|strictest, candidate| {
            if candidate.is_stricter_than(&strictest) {
                candidate
            } else {
                strictest
            }
        })
        .and_then(|lower| build_inferred_retry_constraint(&lower))
}

fn parse_inferred_lower_bound(op: &str, raw_version: &str) -> Option<InferredLowerBound> {
    let (version, precision) = parse_python_version(raw_version)?;
    let inclusive = match op {
        ">=" => true,
        ">" => false,
        _ => return None,
    };
    Some(InferredLowerBound {
        version,
        inclusive,
        precision,
    })
}

fn parse_python_version(raw: &str) -> Option<(semver::Version, VersionPrecision)> {
    let mut parts = raw.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = match parts.next() {
        Some(part) => Some(part.parse::<u64>().ok()?),
        None => None,
    };
    let patch = match parts.next() {
        Some(part) => Some(part.parse::<u64>().ok()?),
        None => None,
    };
    if parts.next().is_some() {
        return None;
    }

    let precision = match (minor, patch) {
        (None, None) => VersionPrecision::Major,
        (Some(_), None) => VersionPrecision::MajorMinor,
        (Some(_), Some(_)) => VersionPrecision::MajorMinorPatch,
        (None, Some(_)) => return None,
    };
    let version = semver::Version::new(major, minor.unwrap_or(0), patch.unwrap_or(0));
    Some((version, precision))
}

fn build_inferred_retry_constraint(lower: &InferredLowerBound) -> Option<InferredRetryConstraint> {
    let upper_version =
        semver::Version::new(lower.version.major, lower.version.minor.checked_add(1)?, 0);
    let upper = InferredUpperBound {
        version: upper_version,
        inclusive: false,
        precision: VersionPrecision::MajorMinor,
    };

    build_retry_constraint(lower.clone(), upper)
}

fn build_retry_constraint(
    lower: InferredLowerBound,
    upper: InferredUpperBound,
) -> Option<InferredRetryConstraint> {
    let request = format!(
        "{}{},{}{}",
        lower.operator(),
        format_lower_bound_version(&lower),
        upper.operator(),
        format_upper_bound_version(&upper)
    );
    let requirement = semver::VersionReq::parse(&format!(
        "{}{},{}{}",
        lower.operator(),
        format_version_for_semver(&lower.version),
        upper.operator(),
        format_version_for_semver(&upper.version)
    ))
    .ok()?;
    let candidate = retry_candidate(&lower)?;

    if !requirement.matches(&candidate) {
        return None;
    }

    Some(InferredRetryConstraint {
        request,
        requirement,
        candidate,
        lower,
        upper,
    })
}

fn retry_candidate(lower: &InferredLowerBound) -> Option<semver::Version> {
    if lower.inclusive {
        return Some(lower.version.clone());
    }

    Some(semver::Version::new(
        lower.version.major,
        lower.version.minor,
        lower.version.patch.checked_add(1)?,
    ))
}

fn format_lower_bound_version(lower: &InferredLowerBound) -> String {
    match lower.precision {
        VersionPrecision::Major => lower.version.major.to_string(),
        VersionPrecision::MajorMinor => format!("{}.{}", lower.version.major, lower.version.minor),
        VersionPrecision::MajorMinorPatch => format!(
            "{}.{}.{}",
            lower.version.major, lower.version.minor, lower.version.patch
        ),
    }
}

fn format_upper_bound_version(upper: &InferredUpperBound) -> String {
    match upper.precision {
        VersionPrecision::Major => upper.version.major.to_string(),
        VersionPrecision::MajorMinor => {
            format!("{}.{}", upper.version.major, upper.version.minor)
        }
        VersionPrecision::MajorMinorPatch => {
            format!(
                "{}.{}.{}",
                upper.version.major, upper.version.minor, upper.version.patch
            )
        }
    }
}

fn format_version_for_semver(version: &semver::Version) -> String {
    format!("{}.{}.{}", version.major, version.minor, version.patch)
}

pub(super) fn retry_request_for_language_request(
    language_request: &LanguageRequest,
    inferred: &InferredRetryConstraint,
) -> Option<String> {
    match language_request {
        LanguageRequest::Any { system_only: false }
        | LanguageRequest::Python(PythonRequest::Any) => Some(inferred.request.clone()),
        LanguageRequest::Any { system_only: true } => None,
        LanguageRequest::Python(PythonRequest::Major(major)) => {
            (inferred.candidate.major == *major).then(|| inferred.request.clone())
        }
        LanguageRequest::Python(PythonRequest::MajorMinor(major, minor)) => {
            (inferred.candidate.major == *major && inferred.candidate.minor == *minor)
                .then(|| inferred.request.clone())
        }
        LanguageRequest::Python(PythonRequest::MajorMinorPatch(major, minor, patch)) => {
            let request = format!("{major}.{minor}.{patch}");
            let version = semver::Version::new(*major, *minor, *patch);
            inferred.requirement.matches(&version).then_some(request)
        }
        LanguageRequest::Python(PythonRequest::Range(requirement, _)) => {
            let (lower, upper) = semver_range_bounds(requirement)?;
            let lower = lower
                .filter(|bound| bound.is_stricter_than(&inferred.lower))
                .unwrap_or_else(|| inferred.lower.clone());
            let upper = upper
                .filter(|bound| bound.is_stricter_than(&inferred.upper))
                .unwrap_or_else(|| inferred.upper.clone());
            build_retry_constraint(lower, upper).map(|constraint| constraint.request)
        }
        _ => None,
    }
}

fn semver_range_bounds(
    requirement: &semver::VersionReq,
) -> Option<(Option<InferredLowerBound>, Option<InferredUpperBound>)> {
    let mut lower = None;
    let mut upper = None;

    for comparator in &requirement.comparators {
        match comparator.op {
            semver::Op::Greater | semver::Op::GreaterEq => {
                let bound = comparator_lower_bound(comparator)?;
                if lower
                    .as_ref()
                    .is_none_or(|current| bound.is_stricter_than(current))
                {
                    lower = Some(bound);
                }
            }
            semver::Op::Less | semver::Op::LessEq => {
                let bound = comparator_upper_bound(comparator)?;
                if upper
                    .as_ref()
                    .is_none_or(|current| bound.is_stricter_than(current))
                {
                    upper = Some(bound);
                }
            }
            _ => return None,
        }
    }

    Some((lower, upper))
}

fn comparator_version(
    comparator: &semver::Comparator,
) -> Option<(semver::Version, VersionPrecision)> {
    let precision = match (comparator.minor, comparator.patch) {
        (None, None) => VersionPrecision::Major,
        (Some(_), None) => VersionPrecision::MajorMinor,
        (Some(_), Some(_)) => VersionPrecision::MajorMinorPatch,
        (None, Some(_)) => return None,
    };
    Some((
        semver::Version::new(
            comparator.major,
            comparator.minor.unwrap_or(0),
            comparator.patch.unwrap_or(0),
        ),
        precision,
    ))
}

fn comparator_lower_bound(comparator: &semver::Comparator) -> Option<InferredLowerBound> {
    let (version, precision) = comparator_version(comparator)?;
    Some(InferredLowerBound {
        version,
        inclusive: comparator.op == semver::Op::GreaterEq,
        precision,
    })
}

fn comparator_upper_bound(comparator: &semver::Comparator) -> Option<InferredUpperBound> {
    let (version, precision) = comparator_version(comparator)?;
    Some(InferredUpperBound {
        version,
        inclusive: comparator.op == semver::Op::LessEq,
        precision,
    })
}

#[cfg(test)]
mod tests {
    use crate::config::Language;
    use crate::languages::version::LanguageRequest;

    #[test]
    fn infer_retry_constraint_parses_python_mismatch() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (3.9.6) does not satisfy Python>=3.10 and example==0.0.0 depends on Python>=3.10, we can conclude that example==0.0.0 cannot be used.
        "};

        let inferred =
            super::infer_retry_constraint(stderr).expect("should infer retry constraint");
        assert_eq!(inferred.request, ">=3.10,<3.11");
    }

    #[test]
    fn infer_retry_constraint_parses_wrapped_python_mismatch() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (3.9.6) does not satisfy
                  Python>=3.10 and example==0.0.0 depends on Python>=3.10, we can conclude that example==0.0.0 cannot be used.
        "};

        let inferred =
            super::infer_retry_constraint(stderr).expect("should infer retry constraint");
        assert_eq!(inferred.request, ">=3.10,<3.11");
    }

    #[test]
    fn infer_retry_constraint_uses_next_minor_cap_for_major_only_bound() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (2.7.18) does not satisfy Python>=3 and example==0.0.0 depends on Python>=3, we can conclude that example==0.0.0 cannot be used.
        "};

        let inferred =
            super::infer_retry_constraint(stderr).expect("should infer retry constraint");
        assert_eq!(inferred.request, ">=3,<3.1");
    }

    #[test]
    fn infer_retry_constraint_uses_strictest_lower_bound() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because the current Python version (3.9.6) does not satisfy Python>=3.10 and package-a==1.0.0 depends on Python>=3.10, we can conclude that package-a==1.0.0 cannot be used.
                Because the current Python version (3.9.6) does not satisfy Python>3.11 and package-b==2.0.0 depends on Python>3.11, we can conclude that package-b==2.0.0 cannot be used.
        "};

        let inferred =
            super::infer_retry_constraint(stderr).expect("should infer retry constraint");
        assert_eq!(inferred.request, ">3.11,<3.12");
    }

    #[test]
    fn infer_retry_constraint_ignores_non_python_resolution_errors() {
        let stderr = indoc::indoc! {r"
            × No solution found when resolving dependencies:
            ╰─▶ Because package-a==1.0.0 depends on package-b==1.0.0 and package-b==2.0.0, we can conclude that package-a==1.0.0 cannot be used.
        "};

        assert!(super::infer_retry_constraint(stderr).is_none());
    }

    #[test]
    fn retry_request_respects_configured_python_request() {
        let inferred = super::infer_retry_constraint(
            "Because the current Python version (3.9.6) does not satisfy Python>=3.10 and x depends on Python>=3.10.",
        )
        .expect("should infer retry constraint");

        let any = LanguageRequest::Any { system_only: false };
        assert_eq!(
            super::retry_request_for_language_request(&any, &inferred),
            Some(">=3.10,<3.11".to_string())
        );

        let system = LanguageRequest::Any { system_only: true };
        assert_eq!(
            super::retry_request_for_language_request(&system, &inferred),
            None
        );

        let compatible_major = LanguageRequest::parse(Language::Python, "3").expect("valid major");
        assert_eq!(
            super::retry_request_for_language_request(&compatible_major, &inferred),
            Some(">=3.10,<3.11".to_string())
        );

        let incompatible_pin =
            LanguageRequest::parse(Language::Python, "3.9").expect("valid major.minor");
        assert_eq!(
            super::retry_request_for_language_request(&incompatible_pin, &inferred),
            None
        );

        let compatible_range =
            LanguageRequest::parse(Language::Python, ">=3.10,<3.10.5").expect("valid range");
        assert_eq!(
            super::retry_request_for_language_request(&compatible_range, &inferred),
            Some(">=3.10,<3.10.5".to_string())
        );

        let incompatible_range =
            LanguageRequest::parse(Language::Python, "<3.10").expect("valid range");
        assert_eq!(
            super::retry_request_for_language_request(&incompatible_range, &inferred),
            None
        );
    }

    #[test]
    fn retry_request_treats_major_minor_as_range_for_exclusive_bound() {
        let inferred = super::infer_retry_constraint(
            "Because the current Python version (3.9.6) does not satisfy Python>3.10 and x depends on Python>3.10.",
        )
        .expect("should infer retry constraint");

        let request = LanguageRequest::parse(Language::Python, "3.10").expect("valid request");
        assert_eq!(
            super::retry_request_for_language_request(&request, &inferred),
            Some(">3.10,<3.11".to_string())
        );
    }

    #[test]
    fn retry_request_keeps_explicit_patch_pin() {
        let inferred = super::infer_retry_constraint(
            "Because the current Python version (3.9.6) does not satisfy Python>=3.10 and x depends on Python>=3.10.",
        )
        .expect("should infer retry constraint");

        let request = LanguageRequest::parse(Language::Python, "3.10.5").expect("valid request");
        assert_eq!(
            super::retry_request_for_language_request(&request, &inferred),
            Some("3.10.5".to_string())
        );
    }

    #[test]
    fn retry_request_refuses_unsupported_range_intersection() {
        let inferred = super::infer_retry_constraint(
            "Because the current Python version (3.9.6) does not satisfy Python>=3.10 and x depends on Python>=3.10.",
        )
        .expect("should infer retry constraint");

        let request = LanguageRequest::parse(Language::Python, "^3.10").expect("valid range");
        assert_eq!(
            super::retry_request_for_language_request(&request, &inferred),
            None
        );
    }
}
