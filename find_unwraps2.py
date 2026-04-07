import re
from pathlib import Path


def get_real_unwraps(d):
    for path in Path(d).rglob("*.rs"):
        with open(path) as f:
            lines = f.readlines()

        in_test_mod = False
        in_test_fn = False
        mod_brace_level = 0
        fn_brace_level = 0
        brace_level = 0

        for i, line in enumerate(lines):
            brace_level += line.count("{")
            brace_level -= line.count("}")

            # Check for mod tests or cfg(test)
            if not in_test_mod:
                if (
                    "mod tests {" in line
                    or "mod test {" in line
                    or "#[cfg(test)]" in line
                ):
                    in_test_mod = True
                    mod_brace_level = brace_level - line.count("{")
            else:
                if brace_level <= mod_brace_level:
                    in_test_mod = False

            # Check for #[test] or #[tokio::test]
            if not in_test_fn and not in_test_mod:
                if "#[test]" in line or "#[tokio::test]" in line:
                    # The function usually starts on the next lines, we need to track brace level when it opens
                    # But for simplicity, let's just say we enter test function and wait for brace level to drop
                    in_test_fn = True
                    fn_brace_level = brace_level  # it will increase when fn opens
            elif in_test_fn:
                # if we have seen the { and now brace_level drops back to where it was before the fn
                if "{" in line and fn_brace_level == brace_level - line.count("{"):
                    fn_brace_level = brace_level - line.count("{")
                elif brace_level <= fn_brace_level:
                    in_test_fn = False

            if (
                not in_test_mod
                and not in_test_fn
                and (".unwrap()" in line or ".expect(" in line)
            ):
                print(f"{path}:{i + 1}:{line.strip()}")


get_real_unwraps("src")
