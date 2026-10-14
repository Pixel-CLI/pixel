// SPDX-FileCopyrightText: The Pixel contributors
// SPDX-License-Identifier: MIT

//! Ruby's core and standard-library names: classes and modules the graph
//! may reopen but whose own methods, superclass and subclasses it does not
//! hold (D31, R5, R11).

/// Ruby's core classes and modules (Ruby 3.3: every module among
/// `Object.constants` under `ruby --disable-gems`), and the standard
/// library's commonly reopened ones, sorted. A class or module of one of
/// these names holds methods and a superclass the graph does not (D31); one
/// nested in it may too, which only makes a lookup abstain unless
/// [`CORE_NESTED`] lists it (R11, R5-10).
const CORE: &[&str] = &[
    "ArgumentError",
    "Array",
    "Base64",
    "BasicObject",
    "Benchmark",
    "BigDecimal",
    "Binding",
    "CSV",
    "Class",
    "ClosedQueueError",
    "Comparable",
    "Complex",
    "ConditionVariable",
    "Data",
    "Date",
    "DateTime",
    "Delegator",
    "Digest",
    "Dir",
    "EOFError",
    "ERB",
    "Encoding",
    "EncodingError",
    "Enumerable",
    "Enumerator",
    "Errno",
    "Exception",
    "FalseClass",
    "Fiber",
    "FiberError",
    "File",
    "FileTest",
    "FileUtils",
    "Float",
    "FloatDomainError",
    "Forwardable",
    "FrozenError",
    "GC",
    "Gem",
    "Hash",
    "IO",
    "IOError",
    "IPAddr",
    "IndexError",
    "Integer",
    "Interrupt",
    "JSON",
    "Kernel",
    "KeyError",
    "LoadError",
    "LocalJumpError",
    "Logger",
    "Marshal",
    "MatchData",
    "Math",
    "Method",
    "Module",
    "Monitor",
    "Mutex",
    "NameError",
    "Net",
    "NilClass",
    "NoMatchingPatternError",
    "NoMatchingPatternKeyError",
    "NoMemoryError",
    "NoMethodError",
    "NotImplementedError",
    "Numeric",
    "Object",
    "ObjectSpace",
    "Open3",
    "OpenSSL",
    "OpenStruct",
    "OptionParser",
    "Pathname",
    "Proc",
    "Process",
    "Psych",
    "Queue",
    "Ractor",
    "Random",
    "Range",
    "RangeError",
    "Rational",
    "Refinement",
    "Regexp",
    "RegexpError",
    "RubyVM",
    "RuntimeError",
    "ScriptError",
    "SecureRandom",
    "SecurityError",
    "Set",
    "Shellwords",
    "Signal",
    "SignalException",
    "SimpleDelegator",
    "Singleton",
    "SizedQueue",
    "Socket",
    "StandardError",
    "StopIteration",
    "String",
    "StringIO",
    "Struct",
    "Symbol",
    "SyntaxError",
    "SystemCallError",
    "SystemExit",
    "SystemStackError",
    "Tempfile",
    "Thread",
    "ThreadError",
    "ThreadGroup",
    "Time",
    "Timeout",
    "TracePoint",
    "TrueClass",
    "TypeError",
    "URI",
    "UnboundMethod",
    "UncaughtThrowError",
    "UnicodeNormalize",
    "Warning",
    "YAML",
    "ZeroDivisionError",
    "Zlib",
];

/// Ruby's nested core classes and modules (Ruby 3.3 `--disable-gems`:
/// every named module with `::`, sorted), and with them every `Errno::`
/// class. Other names nested in [`CORE`] hold classes of their own
/// (`Gem::Command`, `Net::HTTP`, `RubyVM::RJIT::Compiler`, an
/// application's `Data::Export`), which may be Ruby's or the application's
/// ([`in_core_namespace`], R11, R5-10).
const CORE_NESTED: &[&str] = &[
    "Encoding::CompatibilityError",
    "Encoding::Converter",
    "Encoding::ConverterNotFoundError",
    "Encoding::InvalidByteSequenceError",
    "Encoding::UndefinedConversionError",
    "Enumerator::ArithmeticSequence",
    "Enumerator::Chain",
    "Enumerator::Generator",
    "Enumerator::Lazy",
    "Enumerator::Producer",
    "Enumerator::Product",
    "Enumerator::Yielder",
    "File::Constants",
    "File::Stat",
    "GC::Profiler",
    "IO::Buffer",
    "IO::Buffer::AccessError",
    "IO::Buffer::AllocationError",
    "IO::Buffer::InvalidatedError",
    "IO::Buffer::LockedError",
    "IO::Buffer::MaskError",
    "IO::EAGAINWaitReadable",
    "IO::EAGAINWaitWritable",
    "IO::EINPROGRESSWaitReadable",
    "IO::EINPROGRESSWaitWritable",
    "IO::TimeoutError",
    "IO::WaitReadable",
    "IO::WaitWritable",
    "Math::DomainError",
    "ObjectSpace::WeakKeyMap",
    "ObjectSpace::WeakMap",
    "Process::GID",
    "Process::Status",
    "Process::Sys",
    "Process::Tms",
    "Process::UID",
    "Process::Waiter",
    "Ractor::ClosedError",
    "Ractor::Error",
    "Ractor::IsolationError",
    "Ractor::MovedError",
    "Ractor::MovedObject",
    "Ractor::RemoteError",
    "Ractor::UnsafeError",
    "Random::Base",
    "Random::Formatter",
    "Regexp::TimeoutError",
    "RubyVM::AbstractSyntaxTree",
    "RubyVM::AbstractSyntaxTree::Node",
    "RubyVM::InstructionSequence",
    "RubyVM::RJIT",
    "Thread::Backtrace",
    "Thread::Backtrace::Location",
    "Thread::ConditionVariable",
    "Thread::Mutex",
    "Thread::Queue",
    "Thread::SizedQueue",
];

/// Whether the methods of the core or standard-library `owner` that the
/// graph does not hold include no hook: Ruby 3.3 defines `included`,
/// `extended`, `prepended`, `append_features`, `prepend_features` or
/// `extend_object` on the singleton class of no module of [`CORE`] but
/// `Singleton` (`included`, `append_features`).
pub(super) fn defines_no_hook(owner: &str) -> bool {
    is_core(owner) && owner.trim_start_matches("::") != "Singleton"
}

/// Whether `owner` is nested in a core or standard-library namespace
/// ([`CORE`]): `URI::Generic` and `Net::HTTP`, which Ruby defines with
/// methods, a superclass and subclasses the graph does not hold, but also
/// an application's `Data::Export`, which is its own (R11). So it is read
/// only where it makes a lookup abstain (R5-10), never where it would let
/// one assume (an expression's root, a hook).
pub(super) fn in_core_namespace(owner: &str) -> bool {
    let owner = owner.trim_start_matches("::");
    owner
        .split_once("::")
        .is_some_and(|(first, _)| CORE.binary_search(&first).is_ok())
}

/// Whether `owner` is a core or standard-library class or module
/// ([`CORE`], [`CORE_NESTED`]).
pub(super) fn is_core(owner: &str) -> bool {
    let owner = owner.trim_start_matches("::");
    CORE.binary_search(&owner).is_ok()
        || CORE_NESTED.binary_search(&owner).is_ok()
        || owner.starts_with("Errno::")
}
