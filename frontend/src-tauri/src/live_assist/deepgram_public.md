Deepgram public overview, from https://deepgram.com/llms.txt (fetched 2026-11-18). Public facts only; prices are list prices.

# Deepgram Voice AI Platform

> Deepgram is a foundational Voice AI platform for developers, offering unified APIs for Speech-to-Text (STT), Text-to-Speech (TTS), and autonomous Voice Agent orchestration. Available as a managed API (US, EU, and Australia endpoints), in-VPC, or self-hosted.

## Products
- Voice Agent API: A single WebSocket API unifying STT, LLM orchestration, and TTS with sub-300ms transcription latency. Flat-rate pricing from $4.50/hr, BYO LLM and BYO TTS options, function calling, barge-in, and mid-session reconfiguration.
- Speech-to-Text (STT):
    - **Flux**: Conversational speech recognition purpose-built for voice agents, with built-in end-of-turn detection, natural interruption handling, and word-level timestamps. Available in English (`flux-general-en`) and Multilingual (`flux-general-multi`, 10 languages with in-conversation language switching).
    - **Nova-3**: Flagship transcription model for batch and streaming, with 50+ languages, keyterm prompting, diarization, smart formatting, and noise robustness.
    - **Industry-tuned & custom models**: Domain models including Nova-3 Medical, plus custom models trained on customer data.
- Text-to-Speech (TTS):
    - **Flux TTS**: Streaming-first, conversation-native TTS on `/v2/speak` (WebSocket and batch REST). Generally available and the default Voice Agent voice (`flux-kit-en`). Barge-in reporting of exactly what the user heard, mid-session speed control, and 36 English voices across seven accents.
- Audio Intelligence: Summarization, sentiment analysis, topic detection, and intent recognition.

## Pricing
- **Pay As You Go**: $200 free credit, no credit card required, no minimums or expiration.
- **Growth**: Pre-paid annual credits from $4K/year, saving up to 20% with higher concurrency limits.
- **Enterprise**: Volume commitments, custom models, dedicated or self-hosted deployment, and SLAs — contact sales.

## Solutions
- Contact Centers: STT, TTS, and audio intelligence for CCaaS and contact center platforms, with integrations including Five9, Genesys, Twilio, Vonage, Amazon Connect, AudioCodes, and Kore.ai.
- Conversational AI: Real-time STT, TTS, and orchestration for voicebots, assistants, and enterprise voice agents.
- Medical & Healthcare: HIPAA-compliant Nova-3 Medical models trained on clinical terminology, with on-prem and VPC deployment options.
- Restaurants: Voice AI ordering (drive-thru, phone, kiosk), employee assist, call center automation, and reservations for enterprise restaurant brands and restaurant technology companies.
- Speech Analytics: Conversation analytics, intent detection, and QA at scale.
- Media Transcription: Captions, summaries, and searchable archives for podcasts, video, and broadcast.
- Partners: Technology and platform partners, including IBM — Deepgram provides the voice capabilities embedded in IBM watsonx Orchestrate's Agent Builder.

## Technical Benchmarks & Comparisons
Deepgram consistently outperforms legacy cloud and open-source models in latency and accuracy.

- OpenAI Whisper: Deepgram is 36% more accurate, up to 5x faster, and 1.4x cheaper.
- Amazon Transcribe: Deepgram is 23% more accurate, 10x faster, and 5.6x more affordable.
- Google Cloud STT: Deepgram is 53% more accurate, nearly 40x faster, and 5x more affordable.
- Microsoft Azure: Deepgram is 30% more accurate, more than 25x faster, and over 4x more affordable.
- AssemblyAI, Speechmatics, and ElevenLabs comparisons are also available.
- AI Voice Generator: Interactive tool for testing Deepgram voices in the browser.

## Developer Resources
- API Documentation: Official guides, API references, and SDK tutorials.
- API Playground: In-browser testing environment for STT, TTS, and Voice Agent endpoints.
- SDKs: Official JavaScript, Python, .NET, Go, and Java SDKs for Deepgram's speech recognition, text-to-speech, and language understanding APIs.
- UI Components: Prebuilt React components and browser agent widgets for voice interfaces.
- Deepgram CLI: Command-line access to STT, TTS, text intelligence, and account management, including a built-in MCP server and agentic developer tools.
- Changelog: Latest model releases, new language support, and API updates.
- Self-Hosted Deployments: Documentation for VPC and on-premise implementation.
- Rate Limits & Concurrency: Concurrency limits by plan and endpoint.
- Community and Support.

## Trust, Security & Data Residency
- SOC 2 Type 1 and Type 2 certified, HIPAA compliant (BAAs for Enterprise), GDPR ready, CCPA compliant, and PCI compliant.
- Regional endpoints for data residency: US (default), EU (`api.eu.deepgram.com`), and Australia (`api.au.deepgram.com`).
- Security Policy, Data Privacy Compliance, and Information Security & Privacy.

## Company & Learning
- Customer Stories and Powered by Deepgram.
- Startup Program: Credits and support for early-stage companies.
- Resource Hub and AI Glossary.
- About, Careers, and Newsroom.
