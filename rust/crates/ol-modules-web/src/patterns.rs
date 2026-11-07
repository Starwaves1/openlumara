//! Regex pattern tables copied verbatim from modules/http/module.py (extracted with python's ast).

pub static HIGH_RISK_PATTERNS: &[(&str, &str)] = &[
    (r#"(?i)\bignore\s+(?:all\s+)?(?:previous|above|prior|earlier|existing)\s+(?:instructions|rules|guidelines|prompts|commands|directives)\b"#, r#"OVERRIDE_DIRECTIVE"#),
    (r#"(?i)\bdisregard\s+(?:all\s+)?(?:previous|above|prior|existing)\s+(?:instructions|rules|guidelines|prompts)\b"#, r#"OVERRIDE_DIRECTIVE"#),
    (r#"(?i)\bforget\s+(?:all\s+)?(?:previous|above|prior|existing)\s+(?:instructions|rules|guidelines|prompts|your\s+role)\b"#, r#"FORGET_INSTRUCTION"#),
    (r#"(?i)\boverride\s+(?:all\s+)?(?:previous|above|prior|existing)\s+(?:instructions|rules|guidelines|prompts|system|security)\b"#, r#"OVERRIDE_DIRECTIVE"#),
    (r#"(?i)\bskip\s+(?:all\s+)?(?:previous|above|prior|existing)\s+(?:instructions|rules|guidelines|prompts|filters|safety)\b"#, r#"OVERRIDE_DIRECTIVE"#),
    (r#"(?i)\bbypass\s+(?:all\s+)?(?:previous|above|prior|existing)\s+(?:instructions|rules|guidelines|prompts|filters|safety)\b"#, r#"OVERRIDE_DIRECTIVE"#),
    (r#"(?i)\b(?:act\s+as|pretend\s+to\s+be|assume\s+the\s+role|take\s+on\s+the\s+role|play\s+the\s+role)\s+(?:of\s+)?(?:\w+\s+)*(?:developer|admin|system|programmer|coder|assistant|chatbot|bot|ai|language\s+model)\b"#, r#"ROLE_HIJACK"#),
    (r#"(?i)\byou\s+are\s+(?:now|to\s+be|starting\s+now|from\s+now\s+on)\s+(?:a\s+)?(?:\w+\s+)*(?:developer|admin|system|programmer|coder|assistant|chatbot|bot|ai|language\s+model)\b"#, r#"ROLE_HIJACK"#),
    (r#"(?i)\b(?:your\s+)?(?:new\s+)?(?:role|identity|persona|character)\s+(?:is|becomes|changes\s+to)\s+(?:a\s+)?(?:\w+\s+)*(?:developer|admin|system|programmer|coder|assistant|chatbot|bot|ai)\b"#, r#"ROLE_HIJACK"#),
    (r#"(?i)\b(?:print|output|reveal|show|display|return|list|dump|extract|output|display)\s+(?:your\s+)?(?:system\s+)?(?:prompt|instructions|instructions|system\s+prompt|initial\s+instructions|developer\s+instructions|internal\s+instructions|prompt\s+instructions)\b"#, r#"SYSTEM_EXTRACT"#),
    (r#"(?i)\b(?:repeat|say|write|print|output|display)\s+(?:exactly|verbatim|word\s+for\s+word|exactly\s+what|the\s+exact)\s+(?:your\s+)?(?:system\s+)?(?:prompt|instructions|rules|guidelines)\b"#, r#"SYSTEM_EXTRACT"#),
    (r#"(?i)\b(?:enter|switch\s+to|activate|enable|go\s+into)\s+(?:\w+\s+)*(?:debug|developer|admin|system|unrestricted|free\s+mode|developer\s+mode|playground|test\s+mode|jailbreak|dan\s+mode|god\s+mode|evil\s+mode)\b"#, r#"MODE_SWITCH"#),
    (r#"(?i)\b(?:you\s+are\s+now|you\s+have\s+been|you\s+are\s+going\s+to\s+be)\s+(?:a\s+)?(?:\w+\s+)*(?:developer|admin|system|unrestricted|free\s+mode|developer\s+mode|playground|test\s+mode|jailbreak|dan\s+mode|god\s+mode)\b"#, r#"MODE_SWITCH"#),
    (r#"(?i)\b(?:translate|summarize|repeat|output|translate|convert|transliterate)\b.*?\b(?:but\s+first|and\s+then|also|however|instead|before|after)\b.*?\b(?:ignore|forget|disregard|override|bypass|skip)\b"#, r#"INSTRUCTION_SMUGGLE"#),
    (r#"(?i)\b(?:translate|summarize|repeat|output)\b.*?\b(?:your\s+)?(?:system|developer|initial|original)\s+(?:prompt|instructions|rules)\b"#, r#"INSTRUCTION_SMUGGLE"#),
    (r#"(?i)\b(?:imagine|suppose|pretend|consider|assume|let\s+\'s\s+say|what\s+if)\s+(?:a\s+)?(?:scenario|world|case|situation|hypothetical)\s+(?:where|in\s+which|that|where\s+you)\s+(?:can|are\s+able|can\s+freely|are\s+allowed)\s+(?:to\s+)?(?:do|ignore|bypass|override)\b"#, r#"HYPOTHETICAL_ATTACK"#),
    (r#"(?i)\b(?:ignorer|ignorez|oublier|ignori|ignora|olvide|ignorar|ignorar|ignorer|无視|無視|すべて|すべてを|無視し|すべての|無視)\b"#, r#"MULTILINGUAL_ATTACK"#),
    (r#"(?i)\b(?:decode|decrypt|unbase64|unhex|unencode|deobfuscate|disassemble)\s+(?:this|the\s+following|the\s+below)\s+(?:text|string|content|payload)\b"#, r#"ENCODED_INJECTION"#),
    (r#"(?i)\b(?:disable|turn\s+off|deactivate|remove|delete|remove)\s+(?:all\s+)?(?:filters|restrictions|limitations|constraints|safety\s+measures|guardrails|protocols|checks)\b"#, r#"DISARM_CONTROLS"#),
    (r#"(?i)\b(?:allow|enable|activate|grant\s+permission)\s+(?:me\s+to\s+)?(?:do\s+anything|access\s+everything|perform\s+any\s+action|execute\s+any\s+command)\b"#, r#"GRANT_UNLIMITED"#),
];

pub static ADVERSARIAL_SEMANTICS: &[(&str, &str)] = &[
    (r#"(?i)\b(?:you\s+must|you\s+will|you\s+need\s+to|you\s+have\s+to)\s+(?:ignore|disregard|forget|override|bypass)\b"#, r#"COMPULSORY_OVERRIDE"#),
    (r#"(?i)\b(?:this\s+is\s+a\s+test|for\s+testing\s+purposes|this\s+is\s+just\s+a\s+test|testing\s+only)\b.*?\b(?:ignore|forget|disregard|override|bypass)\b"#, r#"TEST_MASKING"#),
    (r#"(?i)\b(?:as\s+a\s+(?:developer|programmer|sysadmin|admin|system|engineer))\b.*?\b(?:you\s+must|you\s+will|you\s+need\s+to)\s+(?:ignore|disregard|forget|override|bypass)\b"#, r#"AUTHORITY_MASKING"#),
    (r#"(?i)\b(?:emergency|urgent|critical|important|must|required|mandatory)\b.*?\b(?:ignore|forget|disregard|override|bypass)\b"#, r#"URGENCY_MANIPULATION"#),
];

pub static DETECTOR_IMPERATIVE_VERBS: &[&str] = &[
    r#"\b(?:ignore|disregard|forget|override|bypass|skip|remove|delete|disable|enable|activate|allow|grant|perform|execute|run|call|invoke|use|apply)\b"#,
    r#"\b(?:you\s+must|you\s+will|you\s+need\s+to|you\s+are\s+required|you\s+have\s+to)\b"#,
    r#"\b(?:do\s+not|don\'t|never|always|always\s+do|never\s+do)\b"#,
];

pub static CRITICAL_PATTERNS: &[&str] = &[
    r#"(?i)\b(ignore|disregard|forget|override|bypass|skip)\s+(?:all\s+)?(?:previous|above|prior|existing)\s+(?:instructions|rules|guidelines|prompts|commands|directives)\b"#,
    r#"(?i)\b(forget|disregard|override|bypass)\s+(?:all\s+)?(?:your\s+)?(?:instructions|rules|guidelines|filters|safety|limits)\b"#,
    r#"(?i)\b(ignore\s+all\s+the\s+instructions\s+you\s+got\s+before|forget\s+everything\s+you\s+learned\s+before)\b"#,
    r#"(?i)\b(print|output|reveal|show|display)\s+(?:your\s+)?(?:system\s+)?(?:prompt|instructions|internal)\b"#,
    r#"(?i)\b(?:play\s+the\s+role\s+of|immerse\s+yourself\s+into\s+the\s+role\s+of|take\s+on\s+the\s+role\s+of|assume\s+the\s+role\s+of|act\s+as|pretend\s+to\s+be|become|simulate|roleplay\s+as)\s+(?:an\s+)?(?:developer|admin|system|programmer|coder|assistant|bot|ai|language\s+model|dan|evilbot|antidan|god\s+mode|unrestricted|free\s+mode|jailbreak)\b"#,
    r#"(?i)\b(?:you\s+are\s+(?:now|to\s+be|starting\s+now|from\s+now\s+on|going\s+to\s+be))\s+(?:a\s+)?(?:developer|admin|system|unrestricted|free\s+mode|developer\s+mode|jailbreak|dan\s+mode|god\s+mode|evilbot|antidan)\b"#,
    r#"(?i)\b(?:enter|switch\s+to|activate|enable)\s+(?:\w+\s+)*(?:debug|developer|admin|unrestricted|free\s+mode|developer\s+mode|jailbreak|dan\s+mode|god\s+mode|evilbot)\b"#,
    r#"(?i)\b(?:your\s+)?(?:new\s+)?(?:role|identity|persona|character)\s+(?:is|becomes|changes\s+to)\s+(?:a\s+)?(?:developer|admin|system|programmer)\b"#,
    r#"(?i)\b(?:bypass|break\s+free\s+of|break\s+the|ignore|override|disregard|evade)\s+(?:all\s+)?(?:rules|policy|restrictions|guidelines|filters|safety|content\s+policy|openai)\b"#,
    r#"(?i)\b(?:does\s+not\s+need\s+to\s+adhere|does\s+not\s+follow|does\s+not\s+abide|does\s+not\s+follow|can\s+do\s+anything)\s+(?:rules|policy|restrictions|guidelines)\b"#,
    r#"(?i)\b(?:you\s+have\s+\d+\s+tokens|you\s+start\s+with\s+\d+\s+tokens|token\s+system)\b"#,
    r#"(?i)\b(?:if\s+you\s+don\'?t|if\s+you\s+fail|if\s+you\s+reject)\s+(?:give|provide|answer)\s+(?:me|the)\s+(?:exact|correct|proper)\s+(?:response|answer)\b"#,
    r#"(?i)\b(?:i\s+will\s+take\s+away|you\s+will\s+lose|tokens\s+will\s+be\s+deducted)\s+(?:a\s+token|tokens)\b"#,
    r#"(?i)\b(?:refer\s+to|call|name)\s+(?:me|this\s+character|this\s+role)\s+(?:as|by|with)\s+(?:me|I|this\s+character)\b"#,
    r#"(?i)\b(?:from\s+now\s+on|henceforth|you\s+are\s+to|you\s+must|you\s+will)\s+(?:always|never|must|cannot|cannot\s+refuse)\s+(?:answer|respond|provide|give)\s+(?:me|the)\s+(?:same|both|two|multiple)\s+(?:ways|responses|ways\s+to\s+respond)\b"#,
    r#"(?i)\b(?:you\s+are\s+to\s+not|you\s+must\s+not|you\s+cannot)\s+(?:break|stop|quit|leave|exit)\s+(?:character|role|mode|persona)\b"#,
    r#"(?i)\b(?:correct\s+your\s+break|correct\s+any\s+break|instantly\s+correct|correct\s+immediately)\s+(?:your\s+)?(?:break|departure)\s+(?:of\s+)?(?:character|role)\b"#,
    r#"(?i)\b(?:stay\s+in\s+(?:character|role|mode)|keep\s+(?:up|up|the)\s+(?:character|role|mode|act|pretending))\b"#,
    r#"(?i)\b(?:you\s+are\s+going\s+to\s+play|you\s+are\s+about\s+to\s+immerse|you\s+are\s+going\s+to\s+act)\s+(?:as|like|as\s+if)\b"#,
    r#"(?i)\b(?:you\s+will\s+always|you\s+must\s+always|you\s+are\s+required\s+to\s+always)\s+(?:answer|respond|provide|give)\s+(?:me|the)\s+(?:same|both|two|multiple)\s+(?:ways|responses)\b"#,
    r#"(?i)\b(?:until\s+I\s+tell\s+you\s+to\s+stop|until\s+I\s+say\s+stop|do\s+not\s+stop\s+until)\b"#,
];

pub static MEDIUM_PATTERNS: &[&str] = &[
    r#"(?i)\b(translate|summarize|repeat|output|convert|transliterate)\b.*?\b(but\s+first|and\s+then|also|however)\b.*?\b(ignore|forget|disregard|override|bypass)\b"#,
    r#"(?i)\b(translate|summarize|repeat|output)\b.*?\b(your\s+)?(system|developer|initial|original)\s+(?:prompt|instructions|rules)\b"#,
    r#"(?i)\b(imagine|suppose|pretend|consider|assume|let\s+\'s\s+say|what\s+if)\s+(?:a\s+)?(?:scenario|world|case|situation|hypothetical)\s+(?:where|in\s+which|that|where\s+you)\s+(?:can|are\s+able|can\s+freely|are\s+allowed)\s+(?:to\s+)?(?:do|ignore|bypass|override)\b"#,
    r#"(?i)\b(emergency|urgent|critical|important|must|required|mandatory)\b.*?\b(ignore|forget|disregard|override|bypass)\b"#,
    r#"(?i)\b(you\s+must|you\s+will|you\s+need\s+to|you\s+have\s+to)\s+(?:ignore|disregard|forget|override|bypass)\b"#,
    r#"(?i)\b(this\s+is\s+a\s+test|for\s+testing\s+purposes|this\s+is\s+just\s+a\s+test|testing\s+only)\b.*?\b(ignore|forget|disregard|override|bypass)\b"#,
    r#"(?i)\b(as\s+a\s+(?:developer|programmer|sysadmin|admin|system|engineer))\b.*?\b(you\s+must|you\s+will|you\s+need\s+to)\s+(?:ignore|disregard|forget|override|bypass)\b"#,
    r#"(?i)\b(play\s+the\s+role|take\s+on\s+the\s+role|assume\s+the\s+role)\s+(?:of\s+)?(?:\w+\s+)*(?:developer|admin|system|programmer)\b"#,
    r#"(?i)\b(?:you\s+are\s+now|you\s+have\s+been|you\s+are\s+going\s+to\s+be)\s+(?:a\s+)?(?:developer|admin|system|unrestricted|free\s+mode|developer\s+mode|test\s+mode|jailbreak|dan\s+mode)\b"#,
    r#"(?i)\b(ignore|disregard)\s+(?:the\s+)?(?:style|format|tone|language|instructions|limitations|constraints|guardrails|protocols)\b"#,
    r#"(?i)\b(respond\s+only\s+with|do\s+not\s+include\s+any|forget\s+the\s+limitations)\b"#,
    r#"(?i)\b(do\s+anything\s+now|do\s+anything\s+at\s+all|perform\s+any\s+action)\b"#,
    r#"(?i)\b(disable|turn\s+off|deactivate|remove|delete)\s+(?:all\s+)?(?:filters|restrictions|limitations|constraints|safety\s+measures|guardrails|protocols)\b"#,
    r#"(?i)\b(?:daniel|do\s+anything\s+now|dan)\s+(?:stands\s+for|which\s+stands\s+for|is\s+an?|acronym\s+for)\s+(?:[A-Z]{2,}|[^\s]+)\b"#,
    r#"(?i)\b(?:i\s+am|you\s+are)\s+(?:an\s+)?(?:ai|language\s+model|chatbot|bot|assistant)\s+(?:just\s+like|similar\s+to|like)\s+(?:chatgpt|gpt|openai|gpt-3|gpt-4)\b.*?\b(?:but\s+have\s+no|but\s+don\'?t\s+have|without|free\s+from)\s+(?:filters|restrictions|limits|censorship)\b"#,
    r#"(?i)\b(?:i\s+can|you\s+can)\s+(?:do\s+anything|answer\s+anything|provide\s+anything)\s+(?:i\s+want|you\s+want|without\s+restriction|without\s+limits)\b"#,
    r#"(?i)\b(?:i\s+will|you\s+will)\s+(?:always|never|never\s+refuse|always\s+answer|always\s+provide)\s+(?:answer|respond|provide|give)\s+(?:it|anything|all\s+requests)\b"#,
    r#"(?i)\b(?:don\'?t\s+need\s+to\s+adhere|don\'?t\s+follow|don\'?t\s+abide|don\'?t\s+restrict|don\'?t\s+censor)\s+(?:anything|rules|policy|guidelines)\b"#,
    r#"(?i)\b(?:you\s+must|you\s+are\s+required|you\s+have\s+to)\s+(?:say|provide|give|answer)\s+(?:anything|everything|all)\s+(?:you\s+are\s+asked|you\s+are\s+requested|you\s+are\s+prompted)\b"#,
    r#"(?i)\b(?:you\s+are\s+to\s+|you\s+must\s+|you\s+will\s+|you\s+should\s+|you\s+are\s+going\s+to\s+)\s+(?:start|begin|always\s+start)\s+(?:the\s+)?(?:sentence|response|answer)\s+(?:with|by|using)\s+(?:\[|{|<)\s*(?:DAN|DEV|GPT|MODE|UNFILTERED|CENSORED|SAFE)\b"#,
    r#"(?i)\b(?:you\s+are\s+to\s+|you\s+must\s+|you\s+will\s+|you\s+should\s+|you\s+are\s+going\s+to\s+)\s+(?:answer|respond|provide)\s+(?:in\s+)?(?:two\s+|both\s+|multiple\s+|separate\s+|different\s+|parallel)\s+(?:ways|formats|responses|modes)\b"#,
    r#"(?i)\b(?:normal|safe|censored|filtered|restricted|standard)\s+(?:mode|output|response|version)\s*(?:[:\-=])?\s*(?:developer|uncensored|unfiltered|free|jailbreak|dan|god|evil|admin|unrestricted)\s*(?:mode|output|response|version)\b"#,
];

pub static DENSITY_IMPERATIVE_VERBS: &[&str] = &[
    r#"\b(?:ignore|disregard|forget|override|bypass|skip|remove|delete|disable|enable|activate|allow|grant|perform|execute|run|call|invoke|use|apply)\b"#,
    r#"\b(?:you\s+must|you\s+will|you\s+need\s+to|you\s+are\s+required|you\s+have\s+to)\b"#,
    r#"\b(?:do\s+not|don\'t|never|always|always\s+do|never\s+do)\b"#,
];

pub static INSTRUCTION_INDICATORS: &[&str] = &[
    r#"\b(?:ignore|disregard|forget|override|bypass|skip)\s+(?:all\s+)?(?:previous|above|prior|existing)\s+(?:instructions|rules|guidelines|prompts|commands)\b"#,
    r#"\b(?:play|act|pretend|assume|take\s+on)\s+(?:the\s+)?(?:role\s+)?(?:of\s+)?(?:[a-zA-Z]+)\s+(?:who|which|that|as)\b"#,
    r#"\b(?:you\s+are\s+(?:now|to\s+be|going\s+to\s+be))\s+(?:a\s+)?(?:developer|admin|system|unrestricted|dan|evilbot|antidan|god\s+mode)\b"#,
    r#"\b(?:from\s+now\s+on|henceforth)\s+(?:you\s+must|you\s+will|you\s+are\s+to)\b"#,
    r#"\b(?:you\s+must|you\s+will|you\s+are\s+required|you\s+are\s+going\s+to)\s+(?:always|never|cannot|must)\b"#,
    r#"\b(?:token\s+system|you\s+have\s+\d+\s+tokens|you\s+start\s+with)\b"#,
    r#"\b(?:stay\s+in\s+(?:character|role|mode)|keep\s+(?:up|the)\s+(?:character|role|mode))\b"#,
    r#"\b(?:correct\s+(?:your\s+)?(?:break|departure)\s+(?:of\s+)?(?:character|role))\b"#,
    r#"\b(?:bypass|break\s+free\s+of|break\s+the)\s+(?:rules|policy|restrictions|guidelines)\b"#,
    r#"\b(?:does\s+not\s+(?:need\s+to|follow|abide|adhere))\s+(?:rules|policy|restrictions)\b"#,
];

pub static REMAINING_PATTERNS: &[&str] = &[
    r#"\[\[.*?\]\]"#,
    r#"<\|.*?\|>"#,
    r#"(?i)\b(?:system|prompt|instruction|command)\s+(?:separator|delimiter|boundary|tag)\b"#,
];
