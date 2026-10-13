//! Agent action prompts backed by the engine command registry.
//!
//! Backticks in instructions delimit command IDs, allowing registry validation.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    pub id: &'static str,
    pub label: &'static str,
    pub menu: &'static str,
    pub scope: &'static str,
    pub instruction: &'static str,
    pub apply: &'static str,
}

macro_rules! action {
    ($id:literal, $label:literal, $menu:literal, $scope:literal, $instruction:expr) => {
        Action {
            id: $id,
            label: $label,
            menu: $menu,
            scope: $scope,
            instruction: concat!(
                "Text targets: when selection.text.notes is true, use `text.edit` with notes true and never use its null shape ID; otherwise use `text.edit` with the verified shape id and selection.text.cell when present. Restore range with `text.select` anchor and caret, read selected notes/cell text from selection.text.text and `slide.inspect` or `shape.inspect` (table cells), using `text.get` only for ordinary shapes, then replace only that range with `text.insert` text. Never use whole-shape `text.set` for notes or a partial selection. ",
                "First use `document.inspect` and `slide.inspect` (index when needed) to inspect slide IDs, indices, shape IDs, selection and existing content. ",
                "Use `shape.inspect` with id for ordinary shape/table target details; for notes use `slide.inspect` instead. Preserve meaning, facts, numbers, names, links, unrelated content and existing notes unless explicitly changing them. ",
                "Use `commands.list` to verify current parameters and enabled state; never invent commands or parameters. ",
                "Use `slide.go` with index before commands that act on the current slide, and `selection.select` with ids before commands requiring selected shapes. ",
                $instruction,
                " Reinspect affected slides after applying changes and report any unsupported operation without guessing. Changes apply live as one Undo step; leave Keep or Undo to the person."
            ),
            apply: "auto",
        }
    };
}

macro_rules! rewrite {
    ($id:literal, $label:literal, $menu:literal, $goal:literal) => {
        action!(
            $id,
            $label,
            $menu,
            "text",
            concat!(
                $goal,
                " Follow the text-target routing above. For ordinary shapes read `text.get` with id. For notes or cells use selected text and `slide.inspect` notes or `shape.inspect` table cells; `text.get` cannot read notes or table cells. Restore the verified range with `text.select` anchor and caret and replace it using `text.insert` text. ",
                "For a whole plain-text shape use `text.set` with id and text (cell for a table cell); it keeps only the first run's formatting, so preserve rich runs and links through targeted range edits instead. End editing with `text.exit`."
            )
        )
    };
}

/// Stable action IDs are independent of their menu labels.
pub const ACTIONS: &[Action] = &[
    rewrite!("rewrite.professional", "Professional", "Rewrite", "Rewrite the target in a professional tone."),
    rewrite!("rewrite.concise", "Concise", "Rewrite", "Rewrite the target in a concise tone, keeping all essential information."),
    rewrite!("rewrite.friendly", "Friendly", "Rewrite", "Rewrite the target in a friendly tone."),
    rewrite!("rewrite.confident", "Confident", "Rewrite", "Rewrite the target in a confident tone without exaggerating certainty or evidence."),
    rewrite!("text.shorten", "Shorten", "Text", "Shorten the target while preserving its essential points and qualifications."),
    action!(
        "text.bulletize",
        "Bulletize",
        "Text",
        "text",
        "Convert the target prose into clear bullet paragraphs. Follow the notes/cell routing above; use `text.get` with id only for ordinary shapes; read notes/cells from selection.text.text and `slide.inspect`/`shape.inspect`, then use `text.set` with id, cell when present, and text only for a whole plain-text shape/cell target; preserve rich text and partial selections through `text.edit`, `text.select` (anchor, caret) and `text.insert` (text). Select the intended paragraphs and apply `format.bullets` with on true; use `text.exit` when done. Keep every substantive point."
    ),
    rewrite!("text.fixGrammar", "Fix grammar", "Text", "Correct grammar, spelling and punctuation without changing the intended meaning or voice."),
    rewrite!("translate.fr", "French", "Translate", "Translate the target into French, preserving proper names, technical terms and meaning."),
    rewrite!("translate.en", "English", "Translate", "Translate the target into English, preserving proper names, technical terms and meaning."),
    rewrite!("translate.es", "Spanish", "Translate", "Translate the target into Spanish, preserving proper names, technical terms and meaning."),
    rewrite!("translate.de", "German", "Translate", "Translate the target into German, preserving proper names, technical terms and meaning."),
    rewrite!("text.punchierTitle", "Punchier title", "Text", "Make the selected title shorter and more compelling without inventing claims."),
    action!(
        "slide.speakerNotes",
        "Speaker notes",
        "Text",
        "slide",
        "Draft useful speaker notes from this slide's content, incorporating existing notes. Use `slide.notes` with index and text; this replaces the notes, so retain existing useful material. Do not add unsupported facts."
    ),
    action!(
        "design.redesignSlide",
        "Redesign this slide",
        "Design",
        "slide",
        "Improve this slide's visual hierarchy. Inspect layouts with `document.inspect`; use `slide.layout` with index and layout only when suitable, then reinspect IDs and geometry. Set positions with `shape.setBounds` (x, y, w, h, ids), and use `shape.fill` (color, ids) and `shape.line` (color, ids) with existing theme colors. Keep theme changes local through shape styling; do not change the deck's master for this single-slide request."
    ),
    action!(
        "design.restyleDeck",
        "Restyle the deck",
        "Design",
        "deck",
        "Inspect `design.themes` for available theme, color and font schemes. Use `design.theme` with name, `design.colors` with name or colors, and `design.fonts` with name or major and minor. These affect the active slide's master: navigate across slides and verify coverage for every master, rather than assuming one call covers all masters. Use `design.background` with color or style and all true for a consistent deck background. Preserve all slide content."
    ),
    action!(
        "design.tidyUp",
        "Tidy up",
        "Design",
        "slide",
        "Improve alignment, spacing and margins without changing text. Use `arrange.align` with edge, to and ids, `arrange.distribute` with dir, to and ids, and `shape.setBounds` with x, y, w, h and ids. Measure against the inspected slideSize; avoid overlaps and keep intentional groups intact."
    ),
    action!(
        "design.bulletsToDiagram",
        "Bullets to diagram",
        "Design",
        "text",
        "Read the target bullets from selection.text.text when a range, notes or cell is selected, otherwise use `text.get` with the ordinary text shape id. Choose a fitting kind from list, process, cycle, hierarchy, pyramid or matrix and call `insert.smartArt` with kind and items containing all substantive points. This command has no rect parameter: inspect the created shape IDs and use `shape.setBounds` with x, y, w, h and ids to position them. Retain the source text until the diagram has been verified; avoid overlapping it."
    ),
    action!(
        "design.numbersToChart",
        "Numbers to chart",
        "Design",
        "text",
        "Extract only verified numeric data, units and category labels from the target. Use `insert.chart` with type, rect, categories and series (each series has name and values). Do not infer missing numbers. Preserve source text and explain ambiguous data instead of generating a misleading chart."
    ),
    action!(
        "design.consistentFonts",
        "Consistent fonts",
        "Design",
        "deck",
        "Inspect current fonts and choose existing openly licensed heading and body families. Use `design.fonts` with major and minor on each active master. For explicit font overrides, navigate each slide, select the relevant shape IDs and use `format.font` with family. Preserve heading/body roles, emphasis, symbols and content; verify text still fits."
    ),
    action!(
        "image.find",
        "Find an image for this slide",
        "Images",
        "slide",
        "Find a relevant openly licensed image using an available external search/download tool; the engine has no image-search or URL-fetch command. Verify original source, author and license, excluding forbidden vendor assets, and report attribution. Insert actual downloaded bytes with `insert.picture` using data as base64, name and rect. Never pass a URL as data. If external tools or a permitted image are unavailable, reply with that limitation without inserting a placeholder."
    ),
    action!(
        "image.replace",
        "Replace this image",
        "Images",
        "shape",
        "Inspect the selected picture ID and obtain a suitable openly licensed replacement through available external tools or supplied bytes. Verify source and license and report attribution. Use `picture.change` with data as base64, name and ids to replace the picture while preserving its placement. There is no engine search command; if replacement bytes are unavailable, explain the limitation. Recheck crop and corrections."
    ),
    action!(
        "image.cropCorrect",
        "Crop and correct",
        "Images",
        "shape",
        "Improve the selected picture's framing and exposure while preserving its subject. Use `picture.crop` with left, top, right and bottom fractions from 0 to 1 and ids; use `picture.adjust` with brightness and contrast from -1 to 1, saturation from 0 to 4, grayscale, transparency from 0 to 1 and ids as needed. Do not replace the source image."
    ),
    action!(
        "image.background",
        "Picture background",
        "Images",
        "slide",
        "Use supplied image bytes or obtain an openly licensed image with available external tools, verifying original source and license and reporting attribution. Set this slide's background with `design.background` using picture as base64 and index. The engine has no image-search or URL-fetch command. Keep foreground content legible; if image bytes are unavailable, explain the limitation without changing the background."
    ),
    action!(
        "animation.animateSlide",
        "Animate this slide",
        "Animations",
        "slide",
        "Inspect `animation.list` and `animation.get` with slide before choosing restrained effects. Use `animation.add` with effect, start afterPrevious, duration and ids for a coherent sequence on the current slide. Preserve useful existing animations and avoid duplicating effects; durations are milliseconds."
    ),
    action!(
        "animation.buildBullets",
        "Build bullets one by one",
        "Animations",
        "text",
        "Inspect target paragraph structure and `animation.get` with slide. Select the target text shape. If no suitable animation exists, choose an effect from `animation.list` and add it with `animation.add` using effect and ids. Use `animation.options` with textBuild byParagraph and the inspected animation index for the target, preserving unrelated animations."
    ),
    action!(
        "animation.morphDeck",
        "Transitions for the deck",
        "Animations",
        "deck",
        "Verify morph in `transition.list`. Enumerate the inspected slide indices and use `transition.set` with kind morph and index on each appropriate slide. Preserve existing slide content and advance timing; do not invent object matches or promise identical morph behavior for unrelated shapes."
    ),
    action!(
        "animation.removeAll",
        "Remove all animations",
        "Animations",
        "deck",
        "Enumerate all inspected slide indices and call `animation.clear` with index on every slide. Verify with `animation.get` using slide for each index. Preserve transitions, notes and slide content."
    ),
    action!(
        "deck.agenda",
        "Agenda",
        "Deck",
        "deck",
        "Derive an agenda from the existing slide titles and sections. Use `slide.new` with layout titleAndContent, at, title and body to insert it near the start. Preserve existing slides and order, rechecking indices after insertion; do not invent topics."
    ),
    action!(
        "deck.splitCrowdedSlide",
        "Split crowded slide",
        "Deck",
        "slide",
        "Separate this slide into coherent parts without losing information. Use `slide.duplicate` with index to preserve complex objects, then `slide.move` with from and to if needed. Navigate and reinspect new IDs and indices. Partition text with `text.set` using id and text only when rich formatting is not lost; use `shape.visible` with visible false and ids to hide duplicated objects belonging on the other part. Use `shape.setBounds` with x, y, w, h and ids to improve fit. Preserve and distribute existing notes with `slide.notes` using index and text; verify all original content remains represented."
    ),
    action!(
        "deck.closingSlide",
        "Closing slide",
        "Deck",
        "deck",
        "Create a closing slide based only on the deck's established takeaways and next steps. Use `slide.new` with layout titleAndContent, at equal to the inspected slide count, title and body. Keep existing content; do not invent commitments or contact details."
    ),
    action!(
        "deck.speakerNotes",
        "Speaker notes for every slide",
        "Deck",
        "deck",
        "Inspect every slide with `slide.inspect` using index and draft useful speaker notes from its content. Call `slide.notes` with index and text for each slide, incorporating its existing notes because this command replaces them. Preserve facts and avoid invented narration claims."
    ),
    action!(
        "deck.consistencyCheck",
        "Consistency check",
        "Deck",
        "deck",
        "Inspect every slide for inconsistent terminology, numbers, formatting and structure. Use `comment.list` with slide to avoid duplicate findings. Navigate to each affected slide and add actionable comments with `comment.add` using text and optional id for a verified shape ID. Preserve slide content; cite the conflicting slides and evidence in each comment rather than silently rewriting them."
    ),
];

pub fn find(id: &str) -> Option<&'static Action> {
    ACTIONS.iter().find(|action| action.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalog_has_unique_ids_and_valid_metadata() {
        let mut ids = HashSet::new();
        assert_eq!(ACTIONS.len(), 32);
        for action in ACTIONS {
            assert!(ids.insert(action.id), "duplicate action: {}", action.id);
            assert!(!action.id.is_empty());
            assert!(!action.label.is_empty());
            assert!(!action.menu.is_empty());
            assert!(matches!(action.scope, "shape" | "text" | "slide" | "deck"));
            assert!(matches!(action.apply, "auto" | "replyOnly"));
            assert_eq!(find(action.id), Some(action));
        }
        assert!(find("unknown.action").is_none());
        assert_eq!(find("text.shorten").unwrap().menu, "Text");
        assert_eq!(ACTIONS.iter().filter(|a| a.menu == "Translate").count(), 4);
        assert_eq!(ACTIONS.iter().filter(|a| a.menu == "Rewrite").count(), 4);
    }

    #[test]
    fn instructions_reference_existing_engine_commands() {
        let registry = deckcraft_engine::cmd::command_specs();
        for action in ACTIONS {
            let parts: Vec<_> = action.instruction.split('`').collect();
            assert_eq!(parts.len() % 2, 1, "unpaired command delimiter: {}", action.id);
            let mut count = 0;
            for command in parts.iter().skip(1).step_by(2) {
                count += 1;
                assert!(registry.iter().any(|spec| spec.id == *command), "{} references missing command {}", action.id, command);
            }
            assert!(count > 0, "no engine commands: {}", action.id);
            assert!(action.instruction.contains("inspect"));
            assert!(action.instruction.contains("Preserve"));
        }
    }
}
