//! Curated set of high-precision secret patterns for the put_raw scanner.
//! See ADR-0013 Decision 5: patterns are compile-time, reviewable at PR
//! time, never loaded from disk.

#[derive(Debug, Clone, Copy)]
pub struct TokenPattern {
    pub name: &'static str,
    pub regex: &'static str,
    pub description: &'static str,
}

pub const PATTERNS: &[TokenPattern] = &[
    TokenPattern {
        name: "github_pat",
        regex: r"gh[poshu]_[A-Za-z0-9_]{36,}",
        description: "GitHub personal-access-token format (ghp_, gho_, ghs_, ghu_, ghp_)",
    },
    TokenPattern {
        name: "aws_access_key_id",
        regex: r"AKIA[0-9A-Z]{16}",
        description: "AWS access key ID",
    },
    TokenPattern {
        name: "anthropic_api_key",
        regex: r"sk-ant-(api|admin)-[A-Za-z0-9_-]{40,}",
        description: "Anthropic API or admin key",
    },
    TokenPattern {
        name: "openai_api_key",
        regex: r"sk-(proj-)?[A-Za-z0-9]{48,}",
        description: "OpenAI API key (standard and project)",
    },
    TokenPattern {
        name: "stripe_live_key",
        regex: r"(sk|pk)_live_[A-Za-z0-9]{24,}",
        description: "Stripe live secret or publishable key",
    },
    TokenPattern {
        name: "gcp_service_account_marker",
        regex: r#""type"\s*:\s*"service_account""#,
        description: "GCP service-account JSON marker",
    },
    TokenPattern {
        name: "private_key_pem_header",
        regex: r"-----BEGIN (RSA |EC |OPENSSH |DSA |)PRIVATE KEY-----",
        description: "PEM-encoded private-key header",
    },
];
