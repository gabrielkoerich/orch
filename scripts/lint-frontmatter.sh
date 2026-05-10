#!/bin/bash
# Lint Zola docs front matter in docs/content/
# Ensures every .md file starts with matching TOML (+++) or YAML (---) delimiters.
# Only checks front matter region (lines 1-30), ignoring code blocks.

set -euo pipefail

CONTENT_DIR="docs/content"
ERRORS=0

for file in $(find "$CONTENT_DIR" -name "*.md" 2>/dev/null); do
    # Get first 30 lines for front matter analysis
    front_matter=$(head -n 30 "$file")

    # Get first line
    first_line=$(echo "$front_matter" | head -n 1)

    # Check if file starts with +++ (TOML)
    if [ "$first_line" = "+++" ]; then
        # Find all +++ lines within front matter region (lines 1-30)
        # Exclude lines inside code blocks (between ``` or ```)
        in_code_block=false
        delimiter_count=0
        for line in $front_matter; do
            if echo "$line" | grep -q '^```'; then
                if [ "$in_code_block" = false ]; then
                    in_code_block=true
                else
                    in_code_block=false
                fi
            elif [ "$in_code_block" = false ] && [ "$line" = "+++" ]; then
                delimiter_count=$((delimiter_count + 1))
            fi
        done

        if [ "$delimiter_count" -ne 2 ]; then
            echo "ERROR: $file has mismatched TOML front matter (+ count: $delimiter_count, expected 2)"
            ERRORS=$((ERRORS + 1))
        fi

    # Check if file starts with --- (YAML)
    elif [ "$first_line" = "---" ]; then
        # Count --- lines in front matter region (lines 1-30)
        # Exclude lines inside code blocks
        in_code_block=false
        delimiter_count=0
        while IFS= read -r line; do
            if echo "$line" | grep -q '^```'; then
                if [ "$in_code_block" = false ]; then
                    in_code_block=true
                else
                    in_code_block=false
                fi
            elif [ "$in_code_block" = false ] && [ "$line" = "---" ]; then
                delimiter_count=$((delimiter_count + 1))
            fi
        done <<< "$front_matter"

        if [ "$delimiter_count" -lt 2 ]; then
            echo "ERROR: $file has mismatched YAML front matter (--- count: $delimiter_count, expected at least 2)"
            ERRORS=$((ERRORS + 1))
        fi

    else
        echo "ERROR: $file is missing front matter (no +++ or --- delimiter found)"
        ERRORS=$((ERRORS + 1))
    fi
done

if [ "$ERRORS" -gt 0 ]; then
    echo ""
    echo "Front matter lint failed: $ERRORS file(s) with errors"
    exit 1
fi

echo "Front matter lint passed: all files have valid front matter"
exit 0