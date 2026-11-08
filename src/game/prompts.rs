use super::random;

/// All categories are suitable for general-purpose servers. Romantic is flirtatious,
/// but contains no explicit sexual material. Players always have the option to skip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, poise::ChoiceParameter)]
pub(crate) enum Category {
    #[name = "Casual"]
    Casual,
    #[name = "Personal"]
    Personal,
    #[name = "Romantic"]
    Romantic,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Self::Casual => "Casual",
            Self::Personal => "Personal",
            Self::Romantic => "Romantic",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, poise::ChoiceParameter)]
pub(crate) enum Kind {
    #[name = "Truth"]
    Truth,
    #[name = "Dare"]
    Dare,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Truth => "Truth",
            Self::Dare => "Dare",
        }
    }
}

const CASUAL_TRUTHS: &[&str] = &[
    "What's the funniest misunderstanding you've ever had?",
    "What's your most embarrassing gaming moment?",
    "What's the strangest food combination you actually enjoy?",
    "What fictional character do you relate to most?",
    "Have you ever confidently given someone the wrong answer?",
    "What's a harmless habit you'd be embarrassed to explain?",
    "What's the most ridiculous thing you believed as a child?",
    "Which song do you secretly know every word to?",
    "What is the worst autocorrect mistake you've sent?",
    "What's the most unusual thing you've collected?",
];
const CASUAL_DARES: &[&str] = &[
    "Speak like a medieval knight for the next two minutes.",
    "Describe your day like a dramatic movie trailer.",
    "Give everyone a one-sentence, completely fictional superhero origin story.",
    "Try to make the group laugh using only three words.",
    "Do an impression of a fictional character until someone guesses who.",
    "Invent a ridiculous advertisement for the nearest object.",
    "Make up a short, terrible rhyme about today's weather.",
    "Explain how to make tea as if it's a top-secret mission.",
    "Speak entirely in questions until your next turn.",
    "Give an enthusiastic one-minute review of an imaginary restaurant.",
];
const PERSONAL_TRUTHS: &[&str] = &[
    "What's something you're proud of but rarely mention?",
    "What was a small act of kindness you'll never forget?",
    "What's an opinion you've changed recently?",
    "When did you last feel genuinely brave?",
    "What's a compliment that has stayed with you?",
    "What habit would you most like to improve?",
    "What's a goal you haven't told many people about?",
    "What's something you wish you were better at expressing?",
    "What's a mistake that taught you an important lesson?",
    "What do you value most in a friendship?",
];
const PERSONAL_DARES: &[&str] = &[
    "Give a sincere compliment to a willing player.",
    "Share a song that means something to you, and explain why.",
    "Tell the group about a personal achievement without downplaying it.",
    "Describe a good memory with another player, if they're comfortable with it.",
    "Share one useful thing you learned this week.",
    "Tell the group about a person you admire and why.",
    "Say something kind about your past self.",
    "Recommend a book, film, or game that changed your perspective.",
    "Give an encouraging speech to the group in 30 seconds.",
    "Tell a funny story from a time you made a mistake.",
];
const ROMANTIC_TRUTHS: &[&str] = &[
    "What's the most attractive personality trait to you?",
    "What's your idea of a perfect first date?",
    "What's the sweetest gesture someone has made for you?",
    "Have you ever had a crush on a friend?",
    "What's your most awkward flirting experience?",
    "What's a small romantic gesture you'd appreciate?",
    "Do you prefer grand gestures or everyday affection?",
    "What's a romantic scene from a movie that you love?",
    "What's a sign that someone really understands you?",
    "Have you ever written a love letter?",
];
const ROMANTIC_DARES: &[&str] = &[
    "Deliver your cheesiest pickup line to a willing player.",
    "Describe a dream date in the style of a fairy tale.",
    "Write a two-line love poem about a completely ordinary object.",
    "Give a respectful, genuinely sweet compliment to a willing player.",
    "Act out the world's most awkward fictional proposal.",
    "Invent a romantic comedy title starring two fictional characters.",
    "Make up a serenade to your favorite snack.",
    "Describe your ideal fictional meet-cute in 30 seconds.",
    "Give a dramatic reading of an imaginary love letter.",
    "Explain your ideal date without using the word 'date'.",
];

fn pool(category: Category, kind: Kind) -> &'static [&'static str] {
    match (category, kind) {
        (Category::Casual, Kind::Truth) => CASUAL_TRUTHS,
        (Category::Casual, Kind::Dare) => CASUAL_DARES,
        (Category::Personal, Kind::Truth) => PERSONAL_TRUTHS,
        (Category::Personal, Kind::Dare) => PERSONAL_DARES,
        (Category::Romantic, Kind::Truth) => ROMANTIC_TRUTHS,
        (Category::Romantic, Kind::Dare) => ROMANTIC_DARES,
    }
}

/// Avoid immediately repeating a prompt of the same kind where possible.
pub(crate) fn draw(category: Category, kind: Kind, previous: Option<&str>) -> &'static str {
    let choices = pool(category, kind);
    let previous_index = previous.and_then(|last| choices.iter().position(|item| *item == last));
    if let Some(excluded) = previous_index.filter(|_| choices.len() > 1) {
        let index = random::index(choices.len() - 1);
        choices[if index >= excluded { index + 1 } else { index }]
    } else {
        choices[random::index(choices.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::{Category, Kind, draw, pool};

    #[test]
    fn all_categories_have_truths_and_dares() {
        for category in [Category::Casual, Category::Personal, Category::Romantic] {
            for kind in [Kind::Truth, Kind::Dare] {
                assert!(pool(category, kind).len() >= 5);
                assert!(pool(category, kind).iter().all(|p| !p.is_empty()));
            }
        }
    }

    #[test]
    fn avoid_immediate_repeat() {
        let last = draw(Category::Casual, Kind::Truth, None);
        for _ in 0..50 {
            assert_ne!(draw(Category::Casual, Kind::Truth, Some(last)), last);
        }
    }
}
