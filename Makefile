EMACS ?= emacs
CARGO ?= cargo
ELISP_PACKAGE_DIR ?= $(CURDIR)/.packages
ELISP_DEPS_STAMP := $(ELISP_PACKAGE_DIR)/.browser-session-ready

.PHONY: check helper-check helper-test elisp-check elisp-deps compile test checkdoc package-lint clean

check: helper-check elisp-check

helper-check:
	$(CARGO) fmt --manifest-path Cargo.toml -- --check
	$(CARGO) clippy --manifest-path Cargo.toml --locked -- -D warnings
	$(CARGO) test --manifest-path Cargo.toml --locked
	$(CARGO) build --manifest-path Cargo.toml --locked

helper-test:
	$(CARGO) test --manifest-path Cargo.toml --locked

elisp-check: compile test checkdoc package-lint

elisp-deps: $(ELISP_DEPS_STAMP)

$(ELISP_DEPS_STAMP):
	$(EMACS) -Q --batch --eval "(progn (require 'package) (setq package-user-dir \"$(ELISP_PACKAGE_DIR)\" package-archives '((\"gnu\" . \"https://elpa.gnu.org/packages/\") (\"melpa\" . \"https://melpa.org/packages/\"))) (package-initialize) (unless (and (package-installed-p 'compat) (package-installed-p 'package-lint)) (package-refresh-contents) (dolist (package '(compat package-lint)) (unless (package-installed-p package) (package-install package)))))"
	@mkdir -p "$(ELISP_PACKAGE_DIR)"
	@touch "$@"

compile:
	$(EMACS) -Q --batch -L . -f batch-byte-compile browser-session.el test/browser-session-test.el

test:
	$(EMACS) -Q --batch -L . -l browser-session.el -l test/browser-session-test.el -f ert-run-tests-batch-and-exit

checkdoc:
	$(EMACS) -Q --batch -L . --eval "(progn (require 'checkdoc) (dolist (file '(\"browser-session.el\")) (with-current-buffer (find-file-noselect file) (let ((checkdoc-create-error-function #'error)) (checkdoc-current-buffer t)))))"

package-lint: elisp-deps
	$(EMACS) -Q --batch --eval "(setq package-user-dir \"$(ELISP_PACKAGE_DIR)\")" --eval "(require 'package)" --eval "(package-initialize)" -l package-lint --eval "(setq package-lint-batch-fail-on-warnings t)" -f package-lint-batch-and-exit browser-session.el

clean:
	rm -f *.elc test/*.elc
	$(CARGO) clean --manifest-path Cargo.toml
