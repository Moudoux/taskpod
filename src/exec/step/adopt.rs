//! `--adopt`: record in the undo journal what each `new` step says, for
//! files and lines taskpod wrote before it kept a journal. A file that's
//! there with no record is recorded as one taskpod created, and a line
//! that's there as one taskpod added, so `new` can come out of the task
//! file: undo then does the same from the journal. Nothing else changes, and
//! with `-d` or `-D` only what it would record is shown.

use super::*;

impl<'r> TaskRun<'r> {
    pub(super) fn adopt_task(&mut self, scope: &Scope) -> Result<Outcome> {
        let task = self.task;
        if task.body.keep {
            return Ok(Outcome::Skipped("keep".into()));
        }
        // Where undo would look, adopt does: undo's reading of the guards.
        if let Some(reason) = self.undo_guard(&task.body.guards, scope)? {
            return Ok(Outcome::Skipped(reason));
        }
        self.adopt_block(&task.body, scope, 0)?;
        Ok(if self.changed {
            Outcome::Ran
        } else {
            Outcome::UpToDate
        })
    }

    fn adopt_block(&mut self, block: &'r Block, scope: &Scope, depth: usize) -> Result<()> {
        for step in &block.steps {
            match step {
                Step::File(file) if file.new && !file.keep => {
                    self.adopt_file(file, scope, depth)?
                }
                Step::Line(line) if line.new && !line.keep => {
                    self.adopt_line(line, scope, depth)?
                }
                Step::Call { template, args } => {
                    let call = Call::new(&self.runner.file.templates[template], args, scope)?;
                    let call_scope = scope.child(&call.args);
                    let inner = call_scope.child(&call.vars);
                    let tmp = crate::vars::tmp_vars(&inner, true);
                    let inner = inner.child(&tmp);
                    let result = self.adopt_nested(&call.template.body, &inner, depth);
                    self.remove_tmp(&tmp, result)?;
                }
                Step::Block(inner) => self.adopt_nested(inner, scope, depth)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// A template's body or an inline block, if undo would go into it.
    fn adopt_nested(&mut self, block: &'r Block, scope: &Scope, depth: usize) -> Result<()> {
        if self.undo_guard(&block.guards, scope)?.is_some() {
            return match &block.otherwise {
                Some(other) => self.adopt_nested(other, scope, depth),
                None => Ok(()),
            };
        }
        if block.keep {
            return Ok(());
        }
        self.in_workdir(block, scope, |this| this.adopt_block(block, scope, depth))
    }

    fn adopt_file(&mut self, file: &FileStep, scope: &Scope, depth: usize) -> Result<()> {
        let root = file.sudo || file.owner.is_some();
        let path = self.target(&scope.expand(&file.path)?);
        if !file.dir {
            return self.adopt_path(&path, root, &format!("file {}", path.display()), depth);
        }
        let FileSource::Copy(source) = &file.source else {
            return self.adopt_path(&path, root, &format!("directory {}", path.display()), depth);
        };
        // Each file a `directory ... from` writes.
        let dir = copy_source(source, scope, source_dir(file, &self.runner.file.ctx.dir))?;
        let mut names = Vec::new();
        list_files(&dir, "", &mut names)
            .with_context(|| format!("failed to list {}", dir.display()))?;
        for name in names {
            let path = path.join(&name);
            self.adopt_path(&path, root, &format!("file {}", path.display()), depth)?;
        }
        Ok(())
    }

    fn adopt_path(&mut self, path: &Path, root: bool, label: &str, depth: usize) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let as_root = self.reads_as_root() && root;
        match journal::read(ctx, &task, path, root, as_root)? {
            Entry::Some(_) => self.current(depth, &format!("{label} · in the journal")),
            Entry::Unreadable => {
                self.change(depth, &format!("{label} · record it{UNREAD_JOURNAL}"));
            }
            Entry::None if target::stat(ctx, &task, path, as_root)?.is_none() => {
                self.current(depth, &format!("{label} · not on the system"));
            }
            Entry::None => {
                self.change(depth, &format!("{label} · record that taskpod created it"));
                if self.mode() == Mode::Run {
                    journal::record(ctx, &task, path, root, &Previous::Missing)?;
                }
            }
        }
        Ok(())
    }

    fn adopt_line(&mut self, step: &LineStep, scope: &Scope, depth: usize) -> Result<()> {
        let ctx = &self.runner.file.ctx;
        let task = self.task.name.clone();
        let line = ResolvedLine::new(step, scope, ctx, &self.cwd)?;
        let label = format!("line {}", line.path.display());
        let root = step.sudo;
        let as_root = self.reads_as_root() && root;
        let Some((existing, base)) = self.read_line_files(step, &line, as_root, depth, &label)?
        else {
            return Ok(());
        };
        let Some(old) = existing else {
            self.current(depth, &format!("{label} · no file"));
            return Ok(());
        };
        let ctx = &self.runner.file.ctx;
        let Some(mut journal) = journal::read_lines(ctx, &task, &line.path, root, as_root)? else {
            self.change(depth, &format!("{label} · record it{UNREAD_JOURNAL}"));
            return Ok(());
        };
        let key = line.key();
        if journal.get(&key).is_some() {
            self.current(depth, &format!("{label} · in the journal"));
            return Ok(());
        }
        if !split_lines(&old).0.iter().any(|l| same_line(l, &line.text)) {
            self.current(depth, &format!("{label} · not in the file"));
            return Ok(());
        }
        self.change(
            depth,
            &format!("{label} · record that taskpod added {:?}", line.text),
        );
        // A file made from its base by taskpod, as undo takes `new` to say.
        if journal.lines.is_empty() && line.base.is_some() {
            journal.created = true;
            journal.base = base;
        }
        journal.lines.push(journal::LineRecord {
            key,
            text: line.text.clone(),
            replaced: None,
        });
        if self.mode() == Mode::Run {
            journal::write_lines(ctx, &task, &line.path, root, &journal)?;
        }
        Ok(())
    }
}
