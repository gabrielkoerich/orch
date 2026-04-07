import re
import sys
from pathlib import Path


def get_real_unwraps(d):
    for path in Path(d).rglob("*.rs"):
        with open(path) as f:
            lines = f.readlines()

        in_test = False
        brace_level = 0
        test_brace_level = 0
        for i, line in enumerate(lines):
            # rudimentary brace counting
            brace_level += line.count("{")
            brace_level -= line.count("}")

            if not in_test:
                if (
                    "mod tests {" in line
                    or "mod test {" in line
                    or "#[cfg(test)]" in line
                ):
                    in_test = True
                    test_brace_level = brace_level - line.count(
                        "{"
                    )  # brace level before this block
            else:
                if brace_level <= test_brace_level:
                    in_test = False

            if not in_test and ".unwrap()" in line:
                print(f"{path}:{i + 1}:{line.strip()}")


get_real_unwraps("src")
