use std::io::{BufRead, Write};

use anyhow::{Result, bail};

pub fn prompt_yes_no(
    input: &mut impl BufRead,
    output: &mut impl Write,
    question: &str,
    default: bool,
) -> Result<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        write!(output, "{question} {suffix} ")?;
        output.flush()?;

        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("input closed");
        }
        let answer = line.trim().to_ascii_lowercase();
        if answer.is_empty() {
            return Ok(default);
        }
        match answer.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(output, "Please answer yes or no.")?,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn prompt_yes_no_accepts_default_and_reprompts_invalid_answers() {
        let mut input = Cursor::new("maybe\n\n");
        let mut output = Vec::new();

        assert!(prompt_yes_no(&mut input, &mut output, "Continue?", true).unwrap());

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Please answer yes or no."));
    }

    #[test]
    fn prompt_yes_no_parses_negative_answers() {
        let mut input = Cursor::new("n\n");
        let mut output = Vec::new();

        assert!(!prompt_yes_no(&mut input, &mut output, "Continue?", true).unwrap());
    }
}
