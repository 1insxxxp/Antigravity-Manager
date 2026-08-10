import { useEffect, useMemo, useState } from 'react';
import { CircleCheck, CircleX, FlaskConical, LoaderCircle, X } from 'lucide-react';
import { createPortal } from 'react-dom';
import { useTranslation } from 'react-i18next';
import { Account } from '../../types/account';
import { AccountModelTestResult, testAccountModel } from '../../services/accountService';

interface AccountTestModalProps {
    account: Account | null;
    onClose: () => void;
}

export default function AccountTestModal({ account, onClose }: AccountTestModalProps) {
    const { t } = useTranslation();
    const models = useMemo(
        () => [...(account?.quota?.models || [])]
            .sort((a, b) => b.percentage - a.percentage || a.name.localeCompare(b.name)),
        [account],
    );
    const [model, setModel] = useState('');
    const [testing, setTesting] = useState(false);
    const [result, setResult] = useState<AccountModelTestResult | null>(null);
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        setModel(models[0]?.name || '');
        setResult(null);
        setError(null);
        setTesting(false);
    }, [account?.id, models]);

    if (!account) return null;

    const runTest = async () => {
        if (!model || testing) return;
        setTesting(true);
        setResult(null);
        setError(null);
        try {
            setResult(await testAccountModel(account.id, model));
        } catch (cause) {
            setError(String(cause));
        } finally {
            setTesting(false);
        }
    };

    return createPortal(
        <div className="modal modal-open z-[120]">
            <div className="modal-box max-w-xl rounded-lg bg-white dark:bg-base-100 p-0 overflow-hidden shadow-2xl">
                <header className="flex items-center justify-between px-5 py-4 border-b border-gray-100 dark:border-base-200">
                    <div className="min-w-0">
                        <h3 className="flex items-center gap-2 text-base font-semibold text-gray-900 dark:text-base-content">
                            <FlaskConical className="w-4 h-4 text-cyan-600" />
                            {t('accounts.model_test.title')}
                        </h3>
                        <p className="mt-1 text-xs text-gray-500 truncate">{account.email}</p>
                    </div>
                    <button className="btn btn-sm btn-circle btn-ghost" onClick={onClose} aria-label={t('common.close')}>
                        <X className="w-4 h-4" />
                    </button>
                </header>

                <div className="p-5 space-y-4">
                    <div>
                        <label className="block mb-1.5 text-xs font-medium text-gray-600 dark:text-gray-300">
                            {t('accounts.model_test.model')}
                        </label>
                        <select
                            className="select select-bordered w-full rounded-lg bg-white dark:bg-base-200"
                            value={model}
                            onChange={(event) => {
                                setModel(event.target.value);
                                setResult(null);
                                setError(null);
                            }}
                            disabled={testing || models.length === 0}
                        >
                            {models.map((item) => (
                                <option key={item.name} value={item.name}>
                                    {item.display_name || item.name} ({item.percentage}%)
                                </option>
                            ))}
                        </select>
                        {models.length === 0 && (
                            <p className="mt-2 text-xs text-amber-600 dark:text-amber-400">
                                {t('accounts.model_test.no_models')}
                            </p>
                        )}
                    </div>

                    {(result || error) && (
                        <section className={`border rounded-lg p-4 ${result?.success
                            ? 'border-emerald-200 bg-emerald-50 dark:border-emerald-900/50 dark:bg-emerald-950/20'
                            : 'border-red-200 bg-red-50 dark:border-red-900/50 dark:bg-red-950/20'}`}>
                            <div className="flex items-center gap-2">
                                {result?.success
                                    ? <CircleCheck className="w-4 h-4 text-emerald-600" />
                                    : <CircleX className="w-4 h-4 text-red-600" />}
                                <span className="text-sm font-semibold text-gray-900 dark:text-base-content">
                                    {result?.success ? t('accounts.model_test.success') : t('accounts.model_test.failed')}
                                </span>
                            </div>
                            {result && (
                                <div className="mt-3 grid grid-cols-2 gap-3 text-xs">
                                    <div><span className="text-gray-500">HTTP</span><div className="mt-0.5 font-mono">{result.status ?? '-'}</div></div>
                                    <div><span className="text-gray-500">{t('accounts.model_test.elapsed')}</span><div className="mt-0.5 font-mono">{result.elapsed_ms} ms</div></div>
                                </div>
                            )}
                            <pre className="mt-3 max-h-52 overflow-auto whitespace-pre-wrap break-words text-xs text-gray-700 dark:text-gray-300 font-mono">
                                {error || result?.error || result?.response || t('accounts.model_test.empty_response')}
                            </pre>
                        </section>
                    )}
                </div>

                <footer className="flex justify-end gap-2 px-5 py-4 border-t border-gray-100 dark:border-base-200 bg-gray-50 dark:bg-base-200/50">
                    <button className="btn btn-sm btn-ghost" onClick={onClose}>{t('common.close')}</button>
                    <button
                        className="inline-flex h-9 min-w-28 items-center justify-center gap-2 rounded-lg bg-cyan-600 px-4 text-sm font-medium text-white transition-colors hover:bg-cyan-700 disabled:cursor-not-allowed disabled:bg-cyan-200 disabled:text-white dark:disabled:bg-cyan-950"
                        onClick={runTest}
                        disabled={!model || testing}
                    >
                        {testing ? <LoaderCircle className="w-4 h-4 animate-spin" /> : <FlaskConical className="w-4 h-4" />}
                        {testing ? t('accounts.model_test.testing') : t('accounts.model_test.run')}
                    </button>
                </footer>
            </div>
            <button className="modal-backdrop" onClick={onClose} aria-label={t('common.close')} />
        </div>,
        document.body,
    );
}
