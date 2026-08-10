EMACS ?= emacs
CARGO ?= cargo
PACKAGE_LINT_PATH ?= $(HOME)/.config/emacs/.local/straight/repos/package-lint
COMPAT_PATH ?= $(HOME)/.config/emacs/.local/straight/repos/compat

.PHONY: check helper-check helper-test compile test checkdoc package-lint clean

check: helper-check compile test checkdoc package-lint

helper-check:
	$(CARGO) fmt --manifest-path Cargo.toml -- --check
	$(CARGO) clippy --manifest-path Cargo.toml --locked -- -D warnings
	$(CARGO) test --manifest-path Cargo.toml --locked
	$(CARGO) build --manifest-path Cargo.toml --locked

helper-test:
	$(CARGO) test --manifest-path Cargo.toml --locked

compile:
	$(EMACS) -Q --batch -L . -f batch-byte-compile browser-session.el test/browser-session-test.el

test:
	$(EMACS) -Q --batch -L . -l browser-session.el -l test/browser-session-test.el -f ert-run-tests-batch-and-exit

checkdoc:
	$(EMACS) -Q --batch -L . --eval "(progn (require 'checkdoc) (dolist (file '(\"browser-session.el\")) (with-current-buffer (find-file-noselect file) (let ((checkdoc-create-error-function #'error)) (checkdoc-current-buffer t)))))"

package-lint:
	@if [ ! -f "$(PACKAGE_LINT_PATH)/package-lint.el" ]; then echo "package-lint unavailable at $(PACKAGE_LINT_PATH)" >&2; exit 1; fi
	$(EMACS) -Q --batch -L $(COMPAT_PATH) -L $(PACKAGE_LINT_PATH) -l package-lint --eval "(setq package-lint-batch-fail-on-warnings t)" -f package-lint-batch-and-exit browser-session.el

clean:
	rm -f *.elc test/*.elc
	$(CARGO) clean --manifest-path Cargo.toml
