//! SmolLM3 chat prompt for one user turn (spec §7).
//!
//! Reproduces the pinned `chat_template.jinja` (revision a07cc9a0) for a
//! single user message, no tools and `add_generation_prompt = true`. The
//! template prints the current date; here the date is an explicit input, so
//! every node renders identical text for the same request. CI compares this
//! rendering with `transformers.apply_chat_template`.

/// A single-turn chat request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatPrompt<'a> {
    /// Optional system message (may carry `/think`, `/no_think` or
    /// `/system_override`, as the template defines).
    pub system: Option<&'a str>,
    pub user: &'a str,
    /// Reasoning mode when the system message does not choose one.
    pub thinking: bool,
    /// The template's `Today Date`, formatted `%d %B %Y` (e.g. `06 October 2026`).
    pub today: &'a str,
}

const DEFAULT_THINK: &str = "You are a helpful AI assistant named SmolLM, trained by Hugging Face. Your role as an assistant involves thoroughly exploring questions through a systematic thinking process before providing the final precise and accurate solutions. This requires engaging in a comprehensive cycle of analysis, summarizing, exploration, reassessment, reflection, backtracking, and iteration to develop well-considered thinking process. Please structure your response into two main sections: Thought and Solution using the specified format: <think> Thought section </think> Solution section. In the Thought section, detail your reasoning process in steps. Each step should include detailed considerations such as analysing questions, summarizing relevant findings, brainstorming new ideas, verifying the accuracy of the current steps, refining any errors, and revisiting previous steps. In the Solution section, based on various attempts, explorations, and reflections from the Thought section, systematically present the final solution that you deem correct. The Solution section should be logical, accurate, and concise and detail necessary steps needed to reach the conclusion.\n\n";
const DEFAULT_NO_THINK: &str =
    "You are a helpful AI assistant named SmolLM, trained by Hugging Face.\n\n";

/// Render the prompt text exactly as the pinned template does.
pub fn render(prompt: &ChatPrompt<'_>) -> String {
    let mut thinking = prompt.thinking;
    let mut custom: Option<String> = None;
    if let Some(system) = prompt.system {
        if system.contains("/no_think") {
            thinking = false;
        } else if system.contains("/think") {
            thinking = true;
        }
        custom = Some(
            system
                .replace("/no_think", "")
                .replace("/think", "")
                .trim_end()
                .to_string(),
        );
    }
    let mode = if thinking { "/think" } else { "/no_think" };
    let mut out = String::from("<|im_start|>system\n");
    // The template tests the original system message, not the stripped one.
    let overriding = prompt
        .system
        .is_some_and(|text| text.contains("/system_override"));
    if overriding {
        let text = custom.as_deref().unwrap_or_default();
        out.push_str(text.replace("/system_override", "").trim_end());
        out.push_str("<|im_end|>\n");
    } else {
        out.push_str("## Metadata\n\nKnowledge Cutoff Date: June 2025\n");
        out.push_str("Today Date: ");
        out.push_str(prompt.today);
        out.push('\n');
        out.push_str("Reasoning Mode: ");
        out.push_str(mode);
        out.push_str("\n\n## Custom Instructions\n\n");
        match custom.as_deref() {
            Some(text) if !text.is_empty() => {
                out.push_str(text);
                out.push_str("\n\n");
            }
            _ => out.push_str(if thinking {
                DEFAULT_THINK
            } else {
                DEFAULT_NO_THINK
            }),
        }
        // Without tools the template never closes the system block.
    }
    out.push_str("<|im_start|>user\n");
    out.push_str(prompt.user);
    out.push_str("<|im_end|>\n");
    if thinking {
        out.push_str("<|im_start|>assistant\n");
    } else {
        out.push_str("<|im_start|>assistant\n<think>\n\n</think>\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_no_think_prompt_matches_the_template() {
        let text = render(&ChatPrompt {
            system: None,
            user: "Hi",
            thinking: false,
            today: "06 October 2026",
        });
        assert_eq!(
            text,
            "<|im_start|>system\n## Metadata\n\nKnowledge Cutoff Date: June 2025\nToday Date: 06 October 2026\nReasoning Mode: /no_think\n\n## Custom Instructions\n\nYou are a helpful AI assistant named SmolLM, trained by Hugging Face.\n\n<|im_start|>user\nHi<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n"
        );
    }

    #[test]
    fn system_messages_choose_the_mode_and_can_override() {
        let think = render(&ChatPrompt {
            system: Some("Be brief. /think  "),
            user: "Q",
            thinking: false,
            today: "01 January 2026",
        });
        assert!(think.contains("Reasoning Mode: /think\n"));
        assert!(think.contains("## Custom Instructions\n\nBe brief.\n\n<|im_start|>user"));
        assert!(think.ends_with("<|im_start|>assistant\n"));
        let overridden = render(&ChatPrompt {
            system: Some("Only this. /system_override"),
            user: "Q",
            thinking: false,
            today: "01 January 2026",
        });
        assert!(
            overridden.starts_with("<|im_start|>system\nOnly this.<|im_end|>\n<|im_start|>user\nQ")
        );
    }
}
