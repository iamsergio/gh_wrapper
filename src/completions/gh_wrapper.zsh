# gh_wrapper completions for zsh
# Add to ~/.zshrc, after compinit: eval "$(gh_wrapper completions zsh)"
# Completes `gh_wrapper` and its `ghw` alias.
_gh_wrapper() {
    local -a commands=(
        'merge:Merge a PR once its CI is green'
        'rebase:Rebase a PR onto its base branch on the server'
        'wait:Wait for CI to finish and notify'
        'rerun:Rerun a failed job, run or PR'\''s failed runs'
        'run_failed:Rerun a PR'\''s failed jobs, leaving cancelled ones alone'
        'authorize:Approve Actions runs awaiting approval'
        'runners:List self-hosted runners of the repos in $GHW_CONFIG_FILE'
        'completions:Print the shell completion script'
    )

    if (( CURRENT == 2 )); then
        if [[ $words[CURRENT] == -* ]]; then
            local -a flags=(
                '--actions:Show each PR'\''s checks'
                '--draft:Include draft PRs'
                '--watched:Include PRs in watched repos'
            )
            _describe -t options 'option' flags
        else
            _describe -t commands 'command' commands
        fi
        return
    fi

    case $words[2] in
        merge)
            _arguments '--force[merge right away, skipping CI]' '*:PR URL(s), comma-separated: ' ;;
        wait)
            _arguments '--no-notify[no desktop notification, only the exit status]' '*:PR URL: ' ;;
        rebase|run_failed)
            _arguments '*:PR URL: ' ;;
        rerun)
            _arguments '*:PR, run or job URL(s), comma-separated: ' ;;
        authorize)
            _arguments '*:PR URL(s), comma-separated: ' ;;
        runners)
            ;;
        completions)
            (( CURRENT == 3 )) && _values 'shell' zsh ;;
        *)
            # Listing flags can be combined.
            local -a flags=(--actions --draft --watched)
            _describe -t options 'option' flags ;;
    esac
}

(( $+functions[compdef] )) && compdef _gh_wrapper gh_wrapper ghw
