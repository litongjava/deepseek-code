//! WebFetch hosts that need no approval, as in the reference CLI: public
//! documentation sites. Any other host asks once; "don't ask again" saves a
//! WebFetch(domain:...) rule. The reference CLI's extra rules for its own
//! GitHub repositories are left out.

/// Hosts, or host/path prefixes, from the reference CLI's list.
const PREAPPROVED: &[&str] = &[
    "agentskills.io",
    "angular.io",
    "asp.net",
    "blazor.net",
    "bun.sh",
    "claude.com/docs",
    "claude.dev",
    "cloud.google.com",
    "code.claude.com",
    "cypress.io",
    "d3js.org",
    "devcenter.heroku.com",
    "developer.android.com",
    "developer.apple.com",
    "developer.mozilla.org",
    "dev.mysql.com",
    "dev.wix.com/docs",
    "doc.rust-lang.org",
    "docs.aws.amazon.com",
    "docs.djangoproject.com",
    "docs.flutter.dev",
    "docs.getdbt.com",
    "docs.netlify.com",
    "docs.oracle.com",
    "docs.python.org",
    "docs.spring.io",
    "docs.stripe.com",
    "docs.swift.org",
    "docs.unity.com",
    "docs.unrealengine.com",
    "dotnet.microsoft.com",
    "en.cppreference.com",
    "expressjs.com",
    "fastapi.tiangolo.com",
    "flask.palletsprojects.com",
    "getbootstrap.com",
    "git-scm.com",
    "go.dev/doc",
    "go.dev/ref",
    "gradle.org",
    "graphql.org",
    "hibernate.org",
    "httpd.apache.org",
    "huggingface.co/docs",
    "jestjs.io",
    "jquery.com",
    "jupyter.org",
    "keras.io",
    "kotlinlang.org",
    "kubernetes.io",
    "laravel.com",
    "learn.microsoft.com",
    "matplotlib.org",
    "maven.apache.org",
    "modelcontextprotocol.io",
    "nextjs.org",
    "nginx.org",
    "nodejs.org",
    "numpy.org",
    "pandas.pydata.org",
    "platform.claude.com",
    "prisma.io",
    "pytorch.org",
    "react.dev",
    "reactnative.dev",
    "reactrouter.com",
    "redis.io",
    "redux.js.org",
    "requests.readthedocs.io",
    "ruby-doc.org",
    "scikit-learn.org",
    "selenium.dev",
    "spark.apache.org",
    "symfony.com",
    "tailwindcss.com",
    "threejs.org",
    "tomcat.apache.org",
    "vercel.com/docs",
    "vuejs.org",
    "webpack.js.org",
    "wordpress.org/documentation",
    "www.ansible.com",
    "www.claude.dev",
    "www.docker.com",
    "www.kaggle.com/docs",
    "www.mongodb.com",
    "www.php.net",
    "www.postgresql.org",
    "www.sqlite.org",
    "www.tensorflow.org",
    "www.terraform.io",
    "www.typescriptlang.org",
];

/// The lowercase host of an http(s) URL.
pub fn host(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok().filter(|url| matches!(url.scheme(), "http" | "https"))?;
    url.host_str().map(|host| host.trim_end_matches('.').to_ascii_lowercase())
}

/// Whether `url` is on a pre-approved documentation site.
pub fn preapproved(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else { return false };
    if !matches!(url.scheme(), "http" | "https") { return false }
    let Some(host) = url.host_str().map(|host| host.trim_end_matches('.').to_ascii_lowercase()) else { return false };
    let path = url.path();
    // Encoded slashes, backslashes or dots could step outside a path prefix.
    let lowered = path.to_ascii_lowercase();
    PREAPPROVED.iter().any(|entry| match entry.split_once('/') {
        None => *entry == host,
        Some((entry_host, prefix)) => {
            let prefix = format!("/{prefix}");
            entry_host == host && !["%2f", "%5c", "%2e"].iter().any(|code| lowered.contains(code)) && (path == prefix || path.starts_with(&format!("{prefix}/")))
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn documentation_sites_and_path_prefixes_are_preapproved() {
        assert!(super::preapproved("https://docs.python.org/3/library/os.html"));
        assert!(super::preapproved("https://go.dev/doc/effective_go"));
        assert!(!super::preapproved("https://go.dev/play"));
        assert!(!super::preapproved("https://go.dev/doc%2f..%2fplay"));
        assert!(!super::preapproved("https://evil.docs.python.org.example.com/"));
        assert!(!super::preapproved("https://example.com/"));
        assert!(!super::preapproved("file:///etc/passwd"));
        assert!(!super::preapproved("https://github.com/anthropics/x"));
    }
}
