//! A strict parser for the clause forms of Drive's `q` parameter that the library sends. Anything else is an error,
//! so a client that forgets to escape a quote or a backslash gets Drive's 400 `invalidQuery` instead of a lucky match.

#[derive(Debug, PartialEq, Eq)]
pub enum Clause {
    InParents(String),
    NameEquals(String),
    MimeTypeEquals(String),
    MimeTypeNotEquals(String),
    Trashed(bool),
}

pub fn parse(query: &str) -> Result<Vec<Clause>, String> {
    let mut scanner = Scanner {
        chars: query.chars().collect(),
        position: 0,
    };
    let mut clauses = vec![scanner.clause()?];

    loop {
        scanner.skip_spaces();

        if scanner.at_end() {
            return Ok(clauses);
        }

        scanner.expect_word("and")?;
        clauses.push(scanner.clause()?);
    }
}

struct Scanner {
    chars: Vec<char>,
    position: usize,
}

impl Scanner {
    fn at_end(&self) -> bool {
        self.position >= self.chars.len()
    }

    fn skip_spaces(&mut self) {
        while self.chars.get(self.position).is_some_and(|c| *c == ' ') {
            self.position += 1;
        }
    }

    fn word(&mut self) -> String {
        self.skip_spaces();

        let start = self.position;

        while self.chars.get(self.position).is_some_and(|c| c.is_ascii_alphanumeric()) {
            self.position += 1;
        }

        self.chars[start..self.position].iter().collect()
    }

    fn expect_word(&mut self, expected: &str) -> Result<(), String> {
        let word = self.word();

        if word == expected {
            Ok(())
        } else {
            Err(format!("expected `{expected}`, found `{word}`"))
        }
    }

    fn operator(&mut self) -> Result<String, String> {
        self.skip_spaces();

        let rest: String = self.chars[self.position..].iter().take(2).collect();

        for operator in ["!=", "="] {
            if rest.starts_with(operator) {
                self.position += operator.len();
                return Ok(operator.to_string());
            }
        }

        Err("expected = or !=".to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        self.skip_spaces();

        if self.chars.get(self.position) != Some(&'\'') {
            return Err("expected a quoted string".to_string());
        }

        self.position += 1;

        let mut value = String::new();

        loop {
            let Some(&character) = self.chars.get(self.position) else {
                return Err("unterminated string".to_string());
            };

            self.position += 1;

            match character {
                '\'' => return Ok(value),
                '\\' => {
                    let Some(&escaped) = self.chars.get(self.position) else {
                        return Err("unterminated escape".to_string());
                    };

                    self.position += 1;

                    match escaped {
                        '\\' | '\'' => value.push(escaped),
                        other => return Err(format!("unsupported escape \\{other}")),
                    }
                }
                other => value.push(other),
            }
        }
    }

    fn clause(&mut self) -> Result<Clause, String> {
        self.skip_spaces();

        if self.chars.get(self.position) == Some(&'\'') {
            let folder = self.string()?;

            self.expect_word("in")?;
            self.expect_word("parents")?;
            return Ok(Clause::InParents(folder));
        }

        let field = self.word();

        match field.as_str() {
            "name" => {
                self.expect_equals()?;
                Ok(Clause::NameEquals(self.string()?))
            }
            "mimeType" => {
                let operator = self.operator()?;
                let value = self.string()?;

                if operator == "=" {
                    Ok(Clause::MimeTypeEquals(value))
                } else {
                    Ok(Clause::MimeTypeNotEquals(value))
                }
            }
            "trashed" => {
                self.expect_equals()?;

                match self.word().as_str() {
                    "true" => Ok(Clause::Trashed(true)),
                    "false" => Ok(Clause::Trashed(false)),
                    other => Err(format!("trashed must be true or false, found `{other}`")),
                }
            }
            other => Err(format!("unsupported clause starting with `{other}`")),
        }
    }

    fn expect_equals(&mut self) -> Result<(), String> {
        if self.operator()? == "=" {
            Ok(())
        } else {
            Err("expected =".to_string())
        }
    }
}
