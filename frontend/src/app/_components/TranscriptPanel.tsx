import { VirtualizedTranscriptView } from '@/components/VirtualizedTranscriptView';
import { PermissionWarning } from '@/components/PermissionWarning';
import { Button } from '@/components/ui/button';
import { ButtonGroup } from '@/components/ui/button-group';
import { Copy, GlobeIcon } from 'lucide-react';
import { useTranscripts } from '@/contexts/TranscriptContext';
import { useConfig } from '@/contexts/ConfigContext';
import { useRecordingState } from '@/contexts/RecordingStateContext';
import { usePermissionCheck } from '@/hooks/usePermissionCheck';
import { useInterimTranscripts } from '@/hooks/useInterimTranscripts';
import { ModalType } from '@/hooks/useModalState';
import { useIsLinux } from '@/hooks/usePlatform';
import { useEffect, useMemo } from 'react';

/**
 * TranscriptPanel Component
 *
 * Displays transcript content with controls for copying and language settings.
 * Uses TranscriptContext, ConfigContext, and RecordingStateContext internally.
 */

interface TranscriptPanelProps {
  // indicates stop-processing state for transcripts; derived from backend statuses.
  isProcessingStop: boolean;
  isStopping: boolean;
  showModal: (name: ModalType, message?: string) => void;
  /** Narrow column (AI Assist mode's expanded transcript): use the full width. */
  compact?: boolean;
}

export function TranscriptPanel({
  isProcessingStop,
  isStopping,
  showModal,
  compact = false,
}: TranscriptPanelProps) {
  // Contexts
  const { transcripts, transcriptContainerRef, copyTranscript, followBottom } = useTranscripts();
  const { transcriptModelConfig } = useConfig();
  const { isRecording, isPaused } = useRecordingState();
  const { checkPermissions, isChecking, hasSystemAudio, hasMicrophone } = usePermissionCheck();
  const isLinux = useIsLinux();
  const { lines: interimLines, receiving: interimReceiving } = useInterimTranscripts(isRecording, isPaused);
  // This panel is the scroller: keep the growing live line in view.
  const interimText = interimLines.map((line) => line.text).join('\n');
  useEffect(() => {
    if (interimText) followBottom();
  }, [interimText, followBottom]);
  // A panel mounted mid-call (back from another page, a layout switch, AI
  // Assist mode's expanded transcript) starts at the top: join the bottom if
  // the reader was following it. Same 150 ms wait as TranscriptContext's
  // auto-scroll, so the rows are laid out first.
  useEffect(() => {
    const t = setTimeout(followBottom, 150);
    return () => clearTimeout(t);
  }, [followBottom]);

  // Convert transcripts to segments for virtualized view
  const segments = useMemo(() =>
    transcripts.map(t => ({
      id: t.id,
      timestamp: t.audio_start_time ?? 0,
      endTime: t.audio_end_time,
      text: t.text,
      confidence: t.confidence,
      speaker: t.speaker,
    })),
    [transcripts]
  );

  return (
    <div className="w-full border-r border-gray-200 bg-white flex flex-col overflow-hidden">
      {/* Title area: above the scroller, not sticky inside it. DO NOT move it
          back into the scroller as a sticky header: the recording status bar
          is sticky top-0 in the same scroller, and the two stacked over each
          other once the transcript scrolled (the bar hid Copy, WORK-757). */}
      <div className="flex-shrink-0 bg-white p-4 border-gray-200">
        <div className="flex flex-col space-y-3">
          <div className="flex  flex-col space-y-2">
            <div className="flex justify-center  items-center space-x-2">
              <ButtonGroup>
                {transcripts?.length > 0 && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={copyTranscript}
                    title="Copy Transcript"
                  >
                    <Copy />
                    <span className='hidden md:inline'>
                      Copy
                    </span>
                  </Button>
                )}
                {transcriptModelConfig.provider === "localWhisper" &&
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => showModal('languageSettings')}
                    title="Language"
                  >
                    <GlobeIcon />
                    <span className='hidden md:inline'>
                      Language
                    </span>
                  </Button>
                }
              </ButtonGroup>
            </div>
          </div>
        </div>
      </div>

      <div ref={transcriptContainerRef} className="flex-1 min-h-0 overflow-y-auto">
        {/* Permission Warning - Not needed on Linux */}
        {!isRecording && !isChecking && !isLinux && (
          <div className="flex justify-center px-4 pt-4">
            <PermissionWarning
              hasMicrophone={hasMicrophone}
              hasSystemAudio={hasSystemAudio}
              onRecheck={checkPermissions}
              isRechecking={isChecking}
            />
          </div>
        )}

        {/* Transcript content */}
        <div className="pb-20">
          <div className="flex justify-center">
            <div className={compact ? 'w-full px-3' : 'w-2/3 max-w-[750px]'}>
              <VirtualizedTranscriptView
                segments={segments}
                isRecording={isRecording}
                isPaused={isPaused}
                isProcessing={isProcessingStop}
                isStopping={isStopping}
                enableStreaming={isRecording && !interimReceiving}
                showConfidence={true}
                interimLines={interimLines}
              />
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
