# SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
#
# SPDX-License-Identifier: GPL-3.0-or-later

unpackNpm() {
    local tarball="$1" dest="$2"
    mkdir -p "$dest"
    tar -xf "$tarball" -C "$dest" --strip-components=1 --no-same-owner
    chmod -R u+rwX "$dest"
}

linkDep() {
    local link="$1/node_modules/$2" target="$3"
    # A package may depend on its own name.
    if [ -e "$link" ] || [ -L "$link" ]; then
        return 0
    fi
    mkdir -p "$(dirname "$link")"
    ln -s "$target" "$link"
}

binEntries() {
    local package="$1"
    [ -f "$package/package.json" ] || return 0
    jq -r '
    if (.bin | type) == "string" then [(.name | sub("^@[^/]+/"; "")), .bin] | @tsv
    elif (.bin | type) == "object" then .bin | to_entries[] | [(.key | sub("^@[^/]+/"; "")), .value] | @tsv
    else empty end' "$package/package.json"
}

markBinsExecutable() {
    local package="$1" name path
    while IFS=$'\t' read -r name path; do
        if [ -f "$package/$path" ]; then
            chmod +x "$package/$path"
        fi
    done < <(binEntries "$package")
}

linkBins() {
    local nodeModules="$1" dep name path
    mkdir -p "$nodeModules/.bin"
    for dep in "$nodeModules"/* "$nodeModules"/@*/*; do
        [ -f "$dep/package.json" ] || continue
        while IFS=$'\t' read -r name path; do
            ln -sfn "../${dep#"$nodeModules"/}/$path" "$nodeModules/.bin/$name"
        done < <(binEntries "$dep")
    done
}

scriptHints() {
    local package="$1" log="$2" missing
    if grep -qE 'ENOTFOUND|EAI_AGAIN|getaddrinfo|Could not resolve host|Temporary failure in name resolution' "$log"; then
        echo "hinata: hint: the install scripts of $package may need network access, which the Nix sandbox does not allow; setting it to \"impure\" in hinata.allowBuilds runs them outside the sandbox" >&2
    fi

    missing=$(sed -nE \
        -e "s/.*No package '([^']+)' found.*/\1/p" \
        -e "s/.*Package '?([^' ,]+)'?,? (was not found|required by .*not found).*/\1/p" \
        -e "s/.*fatal error: '?([^' :]+\.h)'?(: No such file or directory| file not found).*/\1/p" \
        -e 's/.*(cannot find|library not found for) -l([^ :]+).*/-l\2/p' \
        -e 's/.*(pkg-config|pkgconf): (command )?not found.*/pkg-config/p' \
        "$log" | sort -u | paste -sd ' ' -)
    if [ -n "$missing" ]; then
        echo "hinata: hint: the install scripts of $package could not find $missing; if nixpkgs provides them, add them to hinata.buildInputs.$package" >&2
    fi
}

runInstallScripts() {
    local package="$1" nodeModules="$2" event script file
    linkBins "$nodeModules"
    mkdir -p "$TMPDIR/hinata-bin"
    printf '#!%s\nexec node "%s" "$@"\n' "$(command -v sh)" "$nodeGyp" > "$TMPDIR/hinata-bin/node-gyp"
    chmod +x "$TMPDIR/hinata-bin/node-gyp"
    (
        set -o pipefail
        cd "$package"
        export PATH="$nodeModules/.bin:$TMPDIR/hinata-bin:$PATH" HOME="$TMPDIR"
        export npm_package_name npm_package_version
        npm_package_name=$(jq -r .name package.json)
        npm_package_version=$(jq -r .version package.json)
        for event in preinstall install postinstall; do
            script=$(jq -r --arg event "$event" '.scripts[$event] // empty' package.json)
            # npm builds packages that have a binding.gyp but no install or preinstall script.
            if [ -z "$script" ] && [ "$event" = install ] && [ -f binding.gyp ] \
                && [ -z "$(jq -r '.scripts.preinstall // empty' package.json)" ]; then
                script="node-gyp rebuild"
            fi
            [ -n "$script" ] || continue

            echo "hinata: $event $npm_package_name: $script"
            npm_lifecycle_event=$event sh -c "$script" 2>&1 | tee "$TMPDIR/hinata-script.log" || {
                status=$?
                scriptHints "$npm_package_name" "$TMPDIR/hinata-script.log"
                exit "$status"
            }
        done
    )

    # Files hard-linked from other store paths belong to another user, which Nix rejects in outputs.
    while IFS= read -r -d '' file; do
        cp --preserve=mode,timestamps "$file" "$file.hinata-copy"
        mv -f "$file.hinata-copy" "$file"
    done < <(find "$package" -type f -links +1 -print0)
}

linkNodeModules() {
    local from="$1" to="$2" entry bin first
    mkdir -p "$to/.bin"
    for entry in "$from"/* "$from"/.[!.]*; do
        if [ "${entry##*/}" = .bin ] || { [ ! -e "$entry" ] && [ ! -L "$entry" ]; }; then
            continue
        fi
        ln -s "$entry" "$to/"
    done
    # /usr/bin/env is not available in every build sandbox.
    for bin in "$from"/.bin/*; do
        [ -e "$bin" ] || continue
        first=
        { IFS= read -r first || true; } < "$bin"
        if [[ $first == "#!/usr/bin/env node"* ]]; then
            printf '#!%s\nexec node "%s" "$@"\n' "$(command -v sh)" "$(readlink -f "$bin")" > "$to/.bin/${bin##*/}"
            chmod +x "$to/.bin/${bin##*/}"
        else
            ln -s "$bin" "$to/.bin/"
        fi
    done
}
