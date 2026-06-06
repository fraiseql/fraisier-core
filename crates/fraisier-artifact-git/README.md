# fraisier-artifact-git

The [`GitArtifact`] adapter: clone a repository at a ref into a versioned staging
directory and activate it with the shared atomic symlink swap (PRD §6.3, the
`git` artifact source). Cloning shells out to `git`, so it inherits the host's
credentials and SSH configuration for private repositories.
