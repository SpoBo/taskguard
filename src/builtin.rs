//! Built-in defaults. They cover the DALP monorepo out of the box (turbo 2,
//! bun, 281 packages) and most JavaScript monorepos with it. Every list can be
//! extended in config; a config pool or label with the same name replaces the
//! built-in one.

/// Long-running commands. Queueing them would hold room forever, so they run
/// straight through, unmeasured. The same idea as tsc-queue's skip of
/// `tsc --watch`.
pub const PASSTHROUGH: &[&str] = &[
    "* --watch",
    "* watch",
    "tsc -w",
    "tsgo -w",
    "bun --watch",
    "bun --hot",
    "vite $",
    "vite dev",
    "vite serve",
    "vite preview",
    "next dev",
    "next start",
    "astro dev",
    "nuxt dev",
    "storybook dev",
    "wrangler dev",
    "nodemon",
    "devenv up",
    "playwright test --ui",
    "playwright --ui",
    "* --keep-open",
    "* --version",
    "* --help",
    "tsc -v",
    "tsc -h",
    "tsc --init",
    "tsc --showConfig",
    "tsc --listFilesOnly",
    "tsgo --init",
    "tsgo --showConfig",
    "tsgo --listFilesOnly",
];

/// Pools with one slot per checkout: jobs that share one database, one set of
/// running services, or one lock file, and so must not run at the same time.
/// Worktrees get separate pools, because they do not share those things.
pub const POOLS: &[(&str, &[&str])] = &[
    ("db", &["drizzle-kit migrate", "drizzle-kit push", "**/run-drizzle.ts", "**/template-db.ts"]),
    (
        "contracts",
        &[
            "hardhat compile",
            "**/hardhat-runtime.ts compile",
            "onchain/**/compile.ts",
            "scripts/compile.ts",
            "forge build",
            "**/dpm.ts build",
            "dpm build",
        ],
    ),
    ("e2e", &["playwright test", "**/run-dapp-e2e.ts", "e2e/**/run.ts", "vitest * --project e2e", "vitest --project e2e"]),
    ("integration", &["**/run-integration.ts", "vitest * --project integration", "vitest --project integration"]),
];

/// Labels for the dashboard and history. They never change scheduling.
/// Kinds of jobs, for the dashboard and for the guess of a first run. A shell
/// script that runs several gets the heaviest one (`HEAVIEST_FIRST`).
pub const LABELS: &[(&str, &[&str])] = &[
    // A whole CI run or a check across the repo: many tasks, often the whole machine.
    ("ci", &["ci", "ci:*", "check:static", "check:packages", "test:integration", "test:coverage"]),
    ("typecheck", &["tsc", "tsgo", "vue-tsc", "**/typescript/bin/tsc"]),
    (
        "lint",
        &[
            "oxlint",
            "eslint",
            "biome",
            "solhint",
            "ast-grep",
            "sg scan",
            "oxfmt --check",
            "prettier --check",
            "publint",
            "attw",
            "helm lint",
            "stylelint",
            "lintspec",
        ],
    ),
    ("test", &["vitest", "bun test", "jest", "hardhat test", "**/hardhat-runtime.ts test", "forge test", "stryker", "mocha", "ava"]),
    (
        "build",
        &[
            "bun build",
            "vite build",
            "next build",
            "tsup",
            "tsdown",
            "rollup",
            "webpack",
            "esbuild",
            "rspack",
            "rsbuild build",
            "astro build",
        ],
    ),
    (
        "codegen",
        &["fumadocs-mdx", "tsr generate", "graphql-codegen", "drizzle-kit generate", "**/generate*.ts", "**/codegen.ts", "wagmi generate"],
    ),
    ("check", &["**/check-*.ts", "**/audit*.ts", "**/lint-*.ts"]),
];

/// Kinds from heavy to light, to pick one for a script that runs several.
pub const HEAVIEST_FIRST: &[&str] = &["ci", "test", "build", "typecheck", "codegen", "lint", "check"];
