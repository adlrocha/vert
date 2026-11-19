//! The agent picker: `M` chooses, while vert runs, the coding agent and
//! model that explain (`x`), answer questions (`a`) and fix (`f`).
//!
//! What it picks lasts until vert exits; flags and environment variables
//! (`VERT_EXPLAIN_AGENT`, `VERT_EXPLAIN_MODEL`) set what vert starts with.
//! Explanations and questions already on their way finish with the agent
//! they were sent to.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Effect, delete_word};
use crate::explain::Explainer;
use crate::explain::agent::{Choice, Pick, Setting};
use crate::fix::Fixer;

/// The picker while it is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    /// The highlighted entry of [`AgentsState::picks`].
    pub selected: usize,
    /// The model as typed so far; blank for the agent's default.
    pub model: String,
}

/// Which agent and model vert asks, and the picker that changes them.
#[derive(Debug, Default)]
pub struct AgentsState {
    /// The agent and model in use.
    pub current: Option<Setting>,
    /// Whether `--explain-cmd` names a command the picker can offer.
    pub command: bool,
    /// The picker, while it is open.
    pub picker: Option<Picker>,
}

impl AgentsState {
    /// What the picker offers: the `--explain-cmd` command first when there
    /// is one, then each agent vert knows.
    #[must_use]
    pub fn picks(&self) -> Vec<Pick> {
        self.command
            .then_some(Pick::Command)
            .into_iter()
            .chain(Choice::ALL.map(Pick::Agent))
            .collect()
    }
}

/// How the picker names `pick`.
#[must_use]
pub const fn pick_name(pick: Pick) -> &'static str {
    match pick {
        Pick::Command => "--explain-cmd",
        Pick::Agent(choice) => choice.name(),
    }
}

/// How `pick` names its models, for the picker to say.
#[must_use]
pub const fn model_hint(pick: Pick) -> &'static str {
    match pick {
        Pick::Command => "given to the command in VERT_EXPLAIN_MODEL",
        Pick::Agent(Choice::Auto) => "named as the agent found names them",
        Pick::Agent(Choice::Claude) => "e.g. sonnet, opus, haiku",
        Pick::Agent(Choice::Codex) => "any model your Codex login offers",
        Pick::Agent(Choice::Pi) => "provider/model, e.g. anthropic/claude-haiku-5-5",
    }
}

impl App {
    /// Records the agent and model vert starts with, and whether a
    /// `--explain-cmd` can be picked.
    pub fn set_agents(&mut self, current: Setting, command: bool) {
        self.agents.current = Some(current);
        self.agents.command = command;
    }

    /// The agents for `setting` (see [`App::set_agents`]) are ready, or why
    /// one isn't: explanations, questions and fixes from now on use them,
    /// and the footer says which.
    pub fn agents_changed(
        &mut self,
        setting: Setting,
        explainer: &Result<Explainer, String>,
        fixer: &Result<Fixer, String>,
    ) {
        self.agents.current = Some(setting);
        let before = self.explain.agent.clone();
        self.set_explainer(explainer);
        if self.explain.agent != before {
            self.explain.forget_answers();
        }
        self.set_fixer(fixer);
        let explains = match explainer {
            Ok(e) => format!("Explaining and asking with {}", e.label()),
            Err(why) => format!("Can't explain: {why}"),
        };
        self.notice = Some(match fixer {
            Ok(f) => format!("{explains}; fixing with {}", f.label()),
            Err(why) => format!("{explains}; can't fix: {why}"),
        });
    }

    /// `M`: opens the picker on the agent and model in use.
    pub(super) fn open_picker(&mut self) -> Vec<Effect> {
        let picks = self.agents.picks();
        let (selected, model) = self
            .agents
            .current
            .as_ref()
            .map_or((0, String::new()), |c| {
                (
                    picks.iter().position(|p| *p == c.pick).unwrap_or(0),
                    c.model.clone().unwrap_or_default(),
                )
            });
        self.agents.picker = Some(Picker { selected, model });
        Vec::new()
    }

    /// Keys while the picker is open: arrows choose the agent, text edits
    /// the model, `⏎` uses them, `esc` closes it unchanged.
    pub(super) fn on_picker_key(&mut self, key: &KeyEvent) -> Vec<Effect> {
        let count = self.agents.picks().len();
        let Some(picker) = self.agents.picker.as_mut() else {
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.agents.picker = None,
            KeyCode::Enter => return self.apply_picker(),
            KeyCode::Right | KeyCode::Down | KeyCode::Tab => {
                picker.selected = (picker.selected + 1) % count;
            }
            KeyCode::Left | KeyCode::Up | KeyCode::BackTab => {
                picker.selected = (picker.selected + count - 1) % count;
            }
            KeyCode::Backspace => {
                picker.model.pop();
            }
            KeyCode::Char('u') if ctrl => picker.model.clear(),
            KeyCode::Char('w') if ctrl => delete_word(&mut picker.model, char::is_whitespace),
            KeyCode::Char(c) if !ctrl => picker.model.push(c),
            _ => {}
        }
        Vec::new()
    }

    /// `⏎` in the picker: asks for the agents it names (see
    /// [`Effect::SetAgent`]).
    fn apply_picker(&mut self) -> Vec<Effect> {
        let picks = self.agents.picks();
        let Some(picker) = self.agents.picker.take() else {
            return Vec::new();
        };
        let Some(&pick) = picks.get(picker.selected) else {
            return Vec::new();
        };
        let model = Some(picker.model.trim().to_owned()).filter(|m| !m.is_empty());
        vec![Effect::SetAgent(Setting { pick, model })]
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::Msg;
    use crate::git::RepoInfo;

    fn app() -> App {
        let (app, _) = App::new(
            RepoInfo {
                name: "r".to_owned(),
                branch: None,
            },
            1,
            1000,
            100,
            false,
        );
        app
    }

    fn press(app: &mut App, code: KeyCode) -> Vec<Effect> {
        app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    #[test]
    fn the_picker_starts_on_what_is_in_use_and_sends_a_choice() {
        let mut a = app();
        a.set_agents(
            Setting {
                pick: Pick::Agent(Choice::Codex),
                model: Some("gpt-5".to_owned()),
            },
            false,
        );
        assert_eq!(
            a.agents.picks(),
            Choice::ALL.map(Pick::Agent).to_vec(),
            "no command to offer"
        );
        press(&mut a, KeyCode::Char('M'));
        assert_eq!(
            a.agents.picker,
            Some(Picker {
                selected: 2,
                model: "gpt-5".to_owned()
            })
        );
        // Keys go to the picker, not the feed underneath.
        assert_eq!(press(&mut a, KeyCode::Char('q')), []);
        press(&mut a, KeyCode::Backspace);
        a.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        typed(&mut a, " opus  extra");
        a.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
        )));
        press(&mut a, KeyCode::Left);
        press(&mut a, KeyCode::Up);
        press(&mut a, KeyCode::Up);
        assert_eq!(
            a.agents.picker.as_ref().map(|p| p.selected),
            Some(3),
            "wraps"
        );
        press(&mut a, KeyCode::Right);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.agents.picker.as_ref().map(|p| p.selected), Some(1));
        assert_eq!(
            press(&mut a, KeyCode::Enter),
            [Effect::SetAgent(Setting {
                pick: Pick::Agent(Choice::Claude),
                model: Some("opus".to_owned()),
            })]
        );
        assert_eq!(a.agents.picker, None);

        // A blank model is the agent's default; esc changes nothing.
        press(&mut a, KeyCode::Char('M'));
        a.update(Msg::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        typed(&mut a, "  ");
        let [Effect::SetAgent(chosen)] = &press(&mut a, KeyCode::Enter)[..] else {
            panic!("a choice")
        };
        assert_eq!(chosen.model, None);
        press(&mut a, KeyCode::Char('M'));
        assert_eq!(press(&mut a, KeyCode::Esc), []);
        assert_eq!(a.agents.picker, None);
    }

    #[test]
    fn a_command_is_offered_first_and_m_is_typed_where_text_is() {
        let mut a = app();
        a.set_agents(
            Setting {
                pick: Pick::Command,
                model: None,
            },
            true,
        );
        assert_eq!(a.agents.picks()[0], Pick::Command);
        press(&mut a, KeyCode::Char('M'));
        assert_eq!(a.agents.picker.as_ref().map(|p| p.selected), Some(0));
        press(&mut a, KeyCode::Esc);
        // In the search box, M is a letter of the query.
        press(&mut a, KeyCode::Char('3'));
        press(&mut a, KeyCode::Char('M'));
        assert_eq!(a.agents.picker, None);
        assert_eq!(a.search.query, "M");
        // Without a setting recorded, it starts on the first pick.
        let mut fresh = app();
        press(&mut fresh, KeyCode::Char('M'));
        assert_eq!(
            fresh.agents.picker,
            Some(Picker {
                selected: 0,
                model: String::new()
            })
        );
        assert_eq!(pick_name(Pick::Command), "--explain-cmd");
        assert!(model_hint(Pick::Agent(Choice::Pi)).contains("provider/model"));
        for pick in fresh.agents.picks() {
            assert_ne!(model_hint(pick), "");
            assert_ne!(pick_name(pick), "");
        }
    }

    #[test]
    fn a_change_of_agents_is_used_and_told() {
        let mut a = app();
        let setting = Setting {
            pick: Pick::Agent(Choice::Pi),
            model: Some("m".to_owned()),
        };
        let explainer = Ok(Explainer {
            model: Some("m".to_owned()),
            ..crate::app::pi_for_tests()
        });
        let fixer = Err("no claude, codex or pi on PATH".to_owned());
        a.explain.agent = Some(Ok("claude".to_owned()));
        a.update(Msg::Explained {
            key: 7,
            result: Ok(crate::explain::Answer {
                explanation: crate::explain::Explanation::validate(r#"{"intent": "x"}"#, &[])
                    .expect("valid"),
                cached: false,
                agent: None,
            }),
        });
        assert!(a.explain.explanation(7).is_some());
        a.agents_changed(setting.clone(), &explainer, &fixer);
        assert_eq!(a.explain.explanation(7), None, "claude's answer isn't pi's");
        assert_eq!(a.agents.current, Some(setting.clone()));
        assert_eq!(a.explain.agent, Some(Ok("pi · m".to_owned())));
        assert_eq!(
            a.fix.agent,
            Some(Err("no claude, codex or pi on PATH".to_owned()))
        );
        assert_eq!(
            a.notice.as_deref(),
            Some("Explaining and asking with pi · m; can't fix: no claude, codex or pi on PATH")
        );
        let fixer = Ok(Fixer {
            agent: crate::explain::agent::Agent::Pi,
            model: Some("m".to_owned()),
            timeout: std::time::Duration::from_secs(1),
            max_excerpt_bytes: 1,
            commit: false,
        });
        a.agents_changed(setting.clone(), &explainer, &fixer);
        a.update(Msg::Explained {
            key: 8,
            result: Ok(crate::explain::Answer {
                explanation: crate::explain::Explanation::validate(r#"{"intent": "x"}"#, &[])
                    .expect("valid"),
                cached: false,
                agent: None,
            }),
        });
        a.agents_changed(setting.clone(), &explainer, &fixer);
        assert!(
            a.explain.explanation(8).is_some(),
            "the same agent keeps them"
        );
        a.agents_changed(setting, &Err("pi is not on PATH".to_owned()), &fixer);
        assert_eq!(
            a.notice.as_deref(),
            Some("Can't explain: pi is not on PATH; fixing with pi, unconfined · m")
        );
        assert!(a.fix.unconfined);
    }
}
